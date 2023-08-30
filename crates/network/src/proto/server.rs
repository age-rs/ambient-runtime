use std::sync::Arc;

use ambient_core::player::get_by_user_id;
use ambient_ecs::{FrozenWorldDiff, WorldDiff, WorldStreamFilter};
use ambient_native_std::{fps_counter::FpsSample, log_result};
use anyhow::Context;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::{debug_span, Instrument};
use uuid::Uuid;

use crate::{
    bytes_ext::BufExt,
    client::NetworkTransport,
    diff_serialization::{DiffSerializer, WorldDiffDeduplicator},
    log_network_result, log_task_result,
    proto::ServerPush,
    server::{
        bi_stream_handlers, create_player_entity_data, datagram_handlers, uni_stream_handlers,
    },
    server::{SharedServerState, MAIN_INSTANCE_ID},
    stream,
};

use super::ClientRequest;

/// The server can be in multiple states depending on what has been received from the client.
///
/// The server starts in the `PendingConnection` state, until
/// the clients sends a `Connect` request.
#[derive(Default, Debug)]
pub enum ServerProtoState {
    #[default]
    PendingConnection,
    Connected(ConnectedClient),
    Disconnected,
}

#[derive(Default, Debug, Clone)]
pub struct PendingConnection {}

#[derive(Clone)]
pub struct ConnectedClient {
    /// The currently connected user id
    ///
    /// Currently a random friendly_id generated by the client
    user_id: Arc<str>,
    pub control_rx: flume::r#async::RecvStream<'static, ServerPush>,
}

impl std::fmt::Debug for ConnectedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectedClient")
            .field("user_id", &self.user_id)
            .finish()
    }
}

/// Holds information relevant for all states of a given connection to a client
pub struct ConnectionData {
    pub(crate) state: SharedServerState,
    pub(crate) diff_tx: flume::Sender<FrozenWorldDiff>,
    /// Unique identifier for this session
    /// Used to declare ownership of the player entity when multiple simultaneous connections are made or reconnected
    pub(crate) connection_id: Uuid,
    pub(crate) conn: Arc<dyn NetworkTransport>,
    pub(crate) world_stream_filter: WorldStreamFilter,
}

impl std::fmt::Debug for ConnectionData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionData")
            .field("diff_tx", &self.diff_tx)
            .field("diff_tx", &self.diff_tx)
            .field("connection_id", &self.connection_id)
            .finish_non_exhaustive()
    }
}

/// Relevant shared information for a single player connection
pub struct Player {
    pub instance: String,
    control_tx: flume::Sender<ServerPush>,
    connection_id: Uuid,
}

impl Player {
    pub fn new_local(instance: impl Into<String>) -> Self {
        let (control_tx, _) = flume::unbounded();

        Self {
            instance: instance.into(),
            control_tx,
            connection_id: Uuid::new_v4(),
        }
    }

    /// Notifies the existing connection handler to shut down
    pub fn abort(&self) {
        self.control_tx.send(ServerPush::Disconnect).ok();
    }
}

impl ServerProtoState {
    /// Processes a client request
    pub fn process_control(
        &mut self,
        data: &ConnectionData,
        frame: ClientRequest,
    ) -> anyhow::Result<()> {
        match (frame, &self) {
            (_, Self::Disconnected) => {
                tracing::info!("Client is disconnected, ignoring control frame");
                Ok(())
            }
            (ClientRequest::Connect(user_id), Self::PendingConnection) => {
                // Connect the user
                tracing::info!("User connected");
                self.process_connect(data, user_id);
                Ok(())
            }
            (ClientRequest::Connect(_), Self::Connected(_)) => {
                tracing::warn!("Client already connected");
                Ok(())
            }
            (ClientRequest::Disconnect, _) => {
                self.process_disconnect(data);
                Ok(())
            }
        }
    }

    #[tracing::instrument(level = "debug")]
    fn process_connect(&mut self, data: &ConnectionData, user_id: String) {
        let mut state = data.state.lock();

        let (control_tx, control_rx) = flume::unbounded();

        let old_player = state.players.insert(
            user_id.clone(),
            Player {
                instance: MAIN_INSTANCE_ID.to_string(),
                control_tx,
                connection_id: data.connection_id,
            },
        );

        let instance = state.instances.get_mut(MAIN_INSTANCE_ID).unwrap();

        // Bring world stream up to the current time
        tracing::debug!("[{}] Broadcasting diffs", user_id);
        instance.broadcast_diffs();
        tracing::debug!("[{}] Creating init diff", user_id);

        let diff = data.world_stream_filter.initial_diff(&instance.world);

        log_result!(data.diff_tx.send(diff.into()));
        tracing::debug!("[{}] Init diff sent", user_id);

        let entity_data = create_player_entity_data(
            data.conn.clone(),
            user_id.clone(),
            data.diff_tx.clone(),
            data.connection_id,
        );

        if let Some(old_player) = old_player {
            old_player.control_tx.send(ServerPush::Disconnect).ok();

            let id = get_by_user_id(&instance.world, &user_id).unwrap();

            instance.world.add_components(id, entity_data).unwrap();

            tracing::info!(user_id, ?id, "Player reconnected");
        } else {
            let id = instance.spawn_player(entity_data);
            tracing::info!(user_id, ?id, "Player connected");
        }

        *self = Self::Connected(ConnectedClient {
            user_id: user_id.into(),
            control_rx: control_rx.into_stream(),
        });
    }

    #[tracing::instrument(level = "debug")]
    pub fn process_disconnect(&mut self, data: &ConnectionData) {
        if let Self::Connected(ConnectedClient { user_id, .. }) = self {
            tracing::info!(?user_id, "User disconnected");
            let mut state = data.state.lock();

            let Some(player) = state.players.get(&**user_id) else {
                tracing::warn!("Attempt to disconnect a client that was not connected");
                return;
            };

            if player.connection_id != data.connection_id {
                tracing::warn!("Lost ownership of player entity, ignoring disconnect");
                return;
            }

            let player = state.players.remove(&**user_id).unwrap();
            tracing::debug!("Despawning the player from world: {:?}", player.instance);
            state
                .instances
                .get_mut(&player.instance)
                .unwrap()
                .despawn_player(user_id);
        } else {
            tracing::warn!("Tried to disconnect a client that was not connected");
        }

        *self = Self::Disconnected;
    }

    /// Returns `true` if the server state is [`Connected`].
    ///
    /// [`Connected`]: ServerState::Connected
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected(..))
    }

    /// Returns `true` if the server state is [`PendingConnection`].
    ///
    /// [`PendingConnection`]: ServerState::PendingConnection
    #[must_use]
    pub fn is_pending_connection(&self) -> bool {
        matches!(self, Self::PendingConnection)
    }

    /// Returns `true` if the server state is [`Disconnected`].
    ///
    /// [`Disconnected`]: ServerState::Disconnected
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::Disconnected)
    }
}

impl ConnectedClient {
    /// Processes an incoming datagram
    #[tracing::instrument(level = "debug", skip(data))]
    pub fn process_datagram(
        &mut self,
        data: &ConnectionData,
        mut payload: Bytes,
    ) -> anyhow::Result<()> {
        let id = payload.try_get_u32()?;

        let ((name, handler), assets) = {
            let mut state = data.state.lock();
            let world = state
                .get_player_world_mut(&self.user_id)
                .context("Failed to get player world")?;
            (
                world
                    .resource(datagram_handlers())
                    .get(&id)
                    .with_context(|| format!("No handler for datagram {id}"))?
                    .clone(),
                state.assets.clone(),
            )
        };

        let _span = debug_span!("handle_datagram", name, id).entered();
        handler(data.state.clone(), assets, &self.user_id, payload);

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(data, stream))]
    pub fn process_uni<R>(&mut self, data: &ConnectionData, mut stream: R)
    where
        R: 'static + Send + Sync + AsyncRead + Unpin,
    {
        let user_id = self.user_id.clone();
        let state = data.state.clone();

        ambient_sys::task::spawn(
            log_task_result(async move {
                let id = stream.read_u32().await?;

                let ((name, handler), assets) = {
                    let mut state = state.lock();
                    let world = state
                        .get_player_world_mut(&user_id)
                        .context("Failed to get player world")?;
                    (
                        world
                            .resource(uni_stream_handlers())
                            .get(&id)
                            .with_context(|| format!("No handler for unistream {id}"))?
                            .clone(),
                        state.assets.clone(),
                    )
                };

                debug_span!("handle_uni", name, id).in_scope(|| {
                    handler(state, assets, &user_id, Box::pin(stream));
                });

                Ok(())
            })
            .instrument(debug_span!("process_uni")),
        );
    }

    #[tracing::instrument(level = "debug", skip(data, send, recv))]
    pub fn process_bi<S, R>(&mut self, data: &ConnectionData, send: S, mut recv: R)
    where
        R: 'static + Send + Sync + Unpin + AsyncRead,
        S: 'static + Send + Sync + Unpin + AsyncWrite,
    {
        let user_id = self.user_id.clone();
        let state = data.state.clone();

        ambient_sys::task::spawn(
            log_task_result(async move {
                let id = recv.read_u32().await?;

                let ((name, handler), assets) = {
                    let mut state = state.lock();
                    let world = state
                        .get_player_world_mut(&user_id)
                        .context("Failed to get player world")?;
                    (
                        world
                            .resource(bi_stream_handlers())
                            .get(&id)
                            .with_context(|| format!("No handler for bistream {id}"))?
                            .clone(),
                        state.assets.clone(),
                    )
                };

                debug_span!("handle_bi", name, id)
                    .in_scope(|| handler(state, assets, &user_id, Box::pin(send), Box::pin(recv)));

                Ok(())
            })
            .instrument(debug_span!("process_bi")),
        );
    }
}

/// Send the server stats over the network
pub async fn handle_stats<S>(
    stream: stream::FramedSendStream<FpsSample, S>,
    stats: impl Stream<Item = FpsSample>,
) where
    S: Unpin + AsyncWrite,
{
    log_network_result!(stats.map(Ok).forward(stream).await);
}

/// Sends the world diffs over the network
pub async fn handle_diffs<S>(
    mut stream: stream::FramedSendStream<WorldDiff, S>,
    diffs_rx: flume::Receiver<FrozenWorldDiff>,
) where
    S: Unpin + AsyncWrite,
{
    let mut deduplicator = WorldDiffDeduplicator::default();
    let mut serializer = DiffSerializer::default();
    #[cfg(debug_assertions)]
    let mut deserializer = DiffSerializer::default();
    while let Ok(diff) = diffs_rx.recv_async().await {
        let msg = {
            let _span = tracing::debug_span!("handle_diffs prep").entered();

            // get all diffs waiting in the channel to clear the queue
            let mut diffs = Vec::with_capacity(diffs_rx.len() + 1);
            diffs.push(diff);
            diffs.extend(diffs_rx.drain());

            // merge them together, deduplicate and serialize
            let merged_diff = FrozenWorldDiff::merge(&diffs);
            let merged_diffs_count = merged_diff.changes.len();
            let final_diff = deduplicator.deduplicate(merged_diff);
            let msg = serializer.serialize(&final_diff).unwrap();
            tracing::trace!(
                diffs_count = diffs.len(),
                merged_diffs_count,
                final_diffs_count = final_diff.changes.len(),
                bytes = msg.len(),
            );
            #[cfg(debug_assertions)]
            {
                let deserialized = deserializer.deserialize(msg.clone());
                debug_assert!(
                    deserialized.is_ok(),
                    "Diff should deserialize as WorldDiff correctly: {:?}",
                    deserialized,
                );
            }
            msg
        };

        let span = tracing::debug_span!("send_world_diff");
        tracing::trace!(diff=?msg);
        if let Err(err) = stream.send_bytes(msg).instrument(span).await {
            tracing::error!(?err, "Failed to send world diff.");
            break;
        }
    }
}
