//! Main derive macro logic

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs::read;

use can_dbc::{ByteOrder, Dbc};
use proc_macro2::TokenStream;
use quote::{TokenStreamExt, quote};
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, Ident, Path, Result};

use crate::signal::SignalInfo;
use crate::{MessageInfo, parse_attr};

/// Data used for codegen
pub(crate) struct DeriveData<'a> {
    /// Name of the struct we are deriving for
    #[allow(dead_code)]
    name: &'a Ident,
    /// The parsed .dbc file
    dbc: Dbc,
    /// All of the messages to derive
    messages: BTreeMap<String, MessageInfo<'a>>,
    /// Additional #[derive(...)] for generated messages, defaults
    /// to `Default`
    derives: Vec<Path>,
}

impl<'a> DeriveData<'a> {
    pub(crate) fn from(input: &'a DeriveInput) -> Result<Self> {
        // load the DBC file
        let dbc_file = parse_attr(&input.attrs, "dbc_file")
            .expect("Missing #[dbc_file = <filename>] attribute");
        let contents = read(&dbc_file)
            .unwrap_or_else(|_| panic!("Could not read {dbc_file}"));
        let contents = str::from_utf8(&contents)
            .unwrap_or_else(|_| panic!("Could not read {dbc_file}"));

        let dbc = match Dbc::try_from(contents) {
            Ok(dbc) => dbc,
            Err(e) => {
                panic!("Unable to parse {dbc_file}: {e:?}");
            }
        };
        let derives =
            if let Some(traits) = parse_attr(&input.attrs, "dbc_derive") {
                traits
                    .split(',')
                    .filter_map(|t| syn::parse_str::<Path>(t).ok())
                    .collect()
            } else {
                vec![]
            };

        // gather all of the messages and associated attributes
        let mut messages: BTreeMap<String, MessageInfo<'_>> =
            BTreeMap::default();
        match &input.data {
            Data::Struct(data) => match &data.fields {
                Fields::Named(fields) => {
                    for field in &fields.named {
                        if let Some(info) =
                            MessageInfo::from_struct_field(&dbc, field)
                        {
                            messages.insert(info.ident.to_string(), info);
                        } else {
                            return Err(syn::Error::new(
                                field.span(),
                                "Unknown message",
                            ));
                        }
                    }
                }
                Fields::Unnamed(_) | Fields::Unit => unimplemented!(),
            },
            Data::Enum(data) => {
                for variant in &data.variants {
                    if let Some(info) =
                        MessageInfo::from_enum_variant(&dbc, variant)
                    {
                        messages.insert(info.ident.to_string(), info);
                    } else {
                        return Err(syn::Error::new(
                            variant.span(),
                            "Unknown message",
                        ));
                    }
                }
            }
            Data::Union(_) => unimplemented!(),
        }

        Ok(Self {
            name: &input.ident,
            dbc,
            messages,
            derives,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn build(self) -> TokenStream {
        let mut out = TokenStream::new();

        for (name, message) in &self.messages {
            let m = self
                .dbc
                .messages
                .get(message.index)
                .unwrap_or_else(|| panic!("Unknown message {name}"));

            let mut signals: Vec<Ident> = vec![];
            let mut types: Vec<Ident> = vec![];
            let mut docs: Vec<String> = vec![];
            let mut infos: Vec<SignalInfo> = vec![];
            let mut values = TokenStream::new();
            for s in &m.signals {
                if !message.use_signal(&s.name) {
                    continue;
                }

                let signal = SignalInfo::new(s, message);
                signals.push(signal.ident.clone());
                types.push(signal.ntype.clone());

                // documentation text
                let endian_string = if s.byte_order == ByteOrder::LittleEndian {
                    "little-endian"
                } else {
                    "big-endian"
                };
                let scale_string = if signal.is_float() {
                    &format!(", scale factor {}", s.factor)
                } else {
                    ""
                };
                let mut doc = format!(
                    "Wire format: {} bit{} starting at bit {}{} ({})\n",
                    s.size,
                    if s.size == 1 { "" } else { "s" },
                    s.start_bit,
                    scale_string,
                    endian_string,
                );

                // value-table constants
                if let Some(descs) =
                    self.dbc.value_descriptions_for_signal(m.id, &s.name)
                {
                    for desc in descs {
                        let santized: String =
                            format!("{}_{}", s.name, desc.description)
                                .to_uppercase()
                                .chars()
                                .filter(|c| c.is_alphanumeric() || c == &'_')
                                .collect();
                        let c = Ident::new(&santized, signal.ident.span());
                        let i = signal.const_ident(f64::from(desc.id as u32));
                        let v = quote! {#i};
                        let t = signal.ntype.clone();
                        values.extend(quote! {
                            pub const #c: #t = #v;
                        });
                        let _ = write!(doc, "\n{c} = {v}\n");
                    }
                }

                infos.push(signal);
                docs.push(doc);
            }

            let id = message.id;
            let extended = message.extended;

            let dlc = m.size as usize;
            let dlc8 = dlc as u8;
            let ident = message.ident;

            // build signal decoders and encoders
            let mut decoders = TokenStream::new();
            let mut encoders = TokenStream::new();
            for info in &infos {
                decoders.append_all(info.gen_decoder());
                encoders.append_all(info.gen_encoder());
            }
            let cycle_time = if let Some(c) = message.cycle_time {
                quote! {
                    pub const CYCLE_TIME: usize = #c;
                }
            } else {
                quote! {}
            };

            let cycle_time_doc = if let Some(c) = message.cycle_time {
                &format!(", cycle time {c}ms")
            } else {
                ""
            };
            let doc = format!(
                "{} ID {} (0x{:X}){}",
                if extended { "Extended" } else { "Standard" },
                id,
                id,
                cycle_time_doc,
            );

            out.append_all(quote! {
                #[automatically_derived]
                #[allow(non_snake_case)]
                #[allow(non_camel_case_types)]
                #[derive(Default)]
            });

            for t in &self.derives {
                out.append_all(quote! {
                    #[derive(#t)]
                });
            }

            out.append_all(quote! {
                #[doc = #doc]
                pub struct #ident {
                    #(
                        #[doc = #docs]
                        pub #signals: #types
                    ),*
                }

                impl #ident {
                    pub const ID: u32 = #id;
                    pub const DLC: u8 = #dlc8;
                    pub const EXTENDED: bool = #extended;
                    #cycle_time
                    #values

                    pub fn decode(&mut self, pdu: &[u8])
                                  -> bool {
                        if pdu.len() != #dlc {
                            return false
                        }
                        #decoders
                        true
                    }

                    pub fn encode(&mut self, pdu: &mut [u8])
                                  -> bool {
                        if pdu.len() != #dlc {
                            return false
                        }
                        #encoders
                        true
                    }
                }

                impl TryFrom<&[u8]> for #ident {
                    type Error = ();
                    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
                        let mut pdu = Self::default(); // TODO: elide
                        if pdu.decode(data) {
                            Ok(pdu)
                        } else {
                            Err(())
                        }
                    }
                }
            });
        }
        out
    }
}
