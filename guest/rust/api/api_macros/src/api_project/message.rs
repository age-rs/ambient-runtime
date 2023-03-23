use super::{
    tree::{Tree, TreeNode},
    util,
};
use ambient_project::Message;
use proc_macro2::{Span, TokenStream};
use quote::quote;

pub fn tree_to_token_stream(
    message_tree: &Tree<Message>,
    api_path: &syn::Path,
) -> anyhow::Result<TokenStream> {
    to_token_stream(
        message_tree.root(),
        api_path,
        &quote! {
            use #api_path::{prelude::*, message::{Message, MessageSerde, MessageSerdeError}};
        },
    )
}

fn to_token_stream(
    node: &TreeNode<Message>,
    api_path: &syn::Path,
    prelude: &TokenStream,
) -> anyhow::Result<TokenStream> {
    util::tree_to_token_stream(
        node,
        api_path,
        prelude,
        |node, api_path, prelude| to_token_stream(node, api_path, prelude),
        |id, message, api_path| {
            let doc_comment = format!("**{}**: {}", message.name, message.description);

            let struct_name = syn::Ident::new(
                &id.split('_')
                    .map(|segment| {
                        let mut c = segment.chars();
                        match c.next() {
                            None => String::new(),
                            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        }
                    })
                    .collect::<String>(),
                Span::call_site(),
            );

            let fields = message
                .fields
                .iter()
                .map(|f| {
                    let name = f.0;
                    f.1.to_token_stream(api_path, true).map(|ty| {
                        quote! { pub #name: #ty }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            let serialize_fields = message.fields.iter().map(|f| {
                let name = f.0;
                quote! { self.#name.serialize_message_part(&mut output)? }
            });

            let deserialize_fields = message
                .fields
                .iter()
                .map(|f| {
                    let name = f.0;
                    f.1.to_token_stream(api_path, true).map(|ty| {
                        quote! { #name: #ty ::deserialize_message_part(&mut input)? }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            Ok(quote! {
                #[derive(Clone, Debug, PartialEq, Eq)]
                #[doc = #doc_comment]
                pub struct #struct_name {
                    #(#fields,)*
                }
                impl Message for #struct_name {
                    fn id() -> &'static str {
                        #id
                    }
                    fn serialize_message(&self) -> Result<Vec<u8>, MessageSerdeError> {
                        let mut output = vec![];
                        #(#serialize_fields;)*
                        Ok(output)
                    }
                    fn deserialize_message(mut input: &[u8]) -> Result<Self, MessageSerdeError> {
                        Ok(Self {
                            #(#deserialize_fields,)*
                        })
                    }
                }
            })
        },
    )
}
