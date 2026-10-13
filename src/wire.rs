//! What the link says once both sides have said hello: gliff's framing (a CBOR message, then
//! the raw bytes it declares) carrying Navette's own messages. A heartbeat shows the sender
//! alive; the clipboard messages are the five of gliff's transfer engine, so its rules (chunk
//! size, ack window, serials) apply unchanged. A message this build does not know is skipped,
//! so a newer peer can add some without breaking an older one.
//!
//! To extend the link without a new [`VERSION`](crate::link::VERSION), add a top-level variant
//! to [`LinkMsg`] with a number never used before, or an optional field (`Option`, a new `#[n]`)
//! at the end of an existing one: an older peer skips the first and ignores the second. Any
//! other change is malformed to an older peer and ends the link, a new variant of a nested enum
//! (`ClipboardItem`, say) included: change the version instead.

use bytes::Bytes;
use gliff_proto::clipboard::validate_files;
use gliff_proto::{ClipboardFile, ClipboardItem, ClipboardMsg};
use minicbor::{Decode, Encode};

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum LinkMsg {
    #[n(0)]
    Heartbeat,
    #[n(1)]
    ClipboardOffer {
        #[n(0)]
        serial: u32,
        #[n(1)]
        mime_types: Vec<String>,
        #[n(2)]
        files: Vec<ClipboardFile>,
    },
    #[n(2)]
    ClipboardRequest {
        #[n(0)]
        id: u32,
        #[n(1)]
        serial: u32,
        #[n(2)]
        item: ClipboardItem,
    },
    /// The chunk of `data_len` bytes follows the message.
    #[n(3)]
    ClipboardData {
        #[n(0)]
        id: u32,
        #[n(1)]
        offset: u64,
        #[n(2)]
        data_len: u32,
        #[n(3)]
        done: bool,
    },
    #[n(4)]
    ClipboardAck {
        #[n(0)]
        id: u32,
        #[n(1)]
        received: u64,
    },
    #[n(5)]
    ClipboardAbort {
        #[n(0)]
        id: u32,
    },
}

impl From<ClipboardMsg> for LinkMsg {
    fn from(msg: ClipboardMsg) -> Self {
        match msg {
            ClipboardMsg::Offer {
                serial,
                mime_types,
                files,
            } => Self::ClipboardOffer {
                serial,
                mime_types,
                files,
            },
            ClipboardMsg::Request { id, serial, item } => {
                Self::ClipboardRequest { id, serial, item }
            }
            ClipboardMsg::Data {
                id,
                offset,
                data_len,
                done,
            } => Self::ClipboardData {
                id,
                offset,
                data_len,
                done,
            },
            ClipboardMsg::Ack { id, received } => Self::ClipboardAck { id, received },
            ClipboardMsg::Abort { id } => Self::ClipboardAbort { id },
        }
    }
}

impl LinkMsg {
    /// The clipboard message this carries, or `None` for a heartbeat.
    pub fn into_clipboard(self) -> Option<ClipboardMsg> {
        Some(match self {
            Self::Heartbeat => return None,
            Self::ClipboardOffer {
                serial,
                mime_types,
                files,
            } => ClipboardMsg::Offer {
                serial,
                mime_types,
                files,
            },
            Self::ClipboardRequest { id, serial, item } => {
                ClipboardMsg::Request { id, serial, item }
            }
            Self::ClipboardData {
                id,
                offset,
                data_len,
                done,
            } => ClipboardMsg::Data {
                id,
                offset,
                data_len,
                done,
            },
            Self::ClipboardAck { id, received } => ClipboardMsg::Ack { id, received },
            Self::ClipboardAbort { id } => ClipboardMsg::Abort { id },
        })
    }
}

/// The most mime types an offer carries, and the longest one: what one copy needs, so that a
/// 1 MiB frame cannot decode into many times its size of strings waiting in a queue.
pub const MAX_MIME_TYPES: usize = 64;
pub const MAX_MIME_LEN: usize = 255;

/// The peer's message, with an offer this machine will not hold (too many or too long mime
/// types, or files `validate_files` refuses: a path out of the spool, too many entries) made
/// empty. The bridge then withdraws the peer's previous copy rather than keep offering it.
pub fn checked(msg: ClipboardMsg) -> ClipboardMsg {
    match msg {
        ClipboardMsg::Offer {
            serial,
            mime_types,
            files,
        } if mime_types.len() > MAX_MIME_TYPES
            || mime_types.iter().any(|m| m.len() > MAX_MIME_LEN)
            || validate_files(&files).is_err() =>
        {
            ClipboardMsg::Offer {
                serial,
                mime_types: Vec::new(),
                files: Vec::new(),
            }
        }
        msg => msg,
    }
}

/// A clipboard message with the chunk that follows it (empty for all but `Data`): what the
/// transfer engine hands over and takes.
pub type Clip = (ClipboardMsg, Bytes);

#[cfg(test)]
mod tests {
    use gliff_transport::Framed;

    use super::*;

    #[test]
    fn every_clipboard_message_survives_the_trip_through_the_link() {
        let messages = [
            ClipboardMsg::Offer {
                serial: 3,
                mime_types: vec!["text/plain;charset=utf-8".into()],
                files: vec![ClipboardFile {
                    path: "notes.txt".into(),
                    size: 12,
                    dir: false,
                }],
            },
            ClipboardMsg::Request {
                id: 5,
                serial: 3,
                item: ClipboardItem::File(0),
            },
            ClipboardMsg::Data {
                id: 5,
                offset: 0,
                data_len: 4,
                done: false,
            },
            ClipboardMsg::Ack { id: 5, received: 4 },
            ClipboardMsg::Abort { id: 5 },
        ];
        for msg in messages {
            let bytes = minicbor::to_vec(LinkMsg::from(msg.clone())).unwrap();
            let back: LinkMsg = minicbor::decode(&bytes).unwrap();
            assert_eq!(back.into_clipboard(), Some(msg));
        }
    }

    #[tokio::test]
    async fn a_message_of_a_newer_peer_is_skipped_with_its_bytes() {
        #[derive(Encode)]
        enum Newer {
            #[n(40)]
            #[allow(dead_code)]
            Unknown(#[n(0)] u32),
        }
        let (a, b) = tokio::io::duplex(4096);
        let (mut write, mut read) = (Framed::new(a), Framed::new(b));
        write
            .write_msg_with_payloads(&Newer::Unknown(1), &[b"ignored"])
            .await
            .unwrap();
        write.write_msg(&LinkMsg::Heartbeat).await.unwrap();
        assert_eq!(
            read.read_msg::<LinkMsg>().await.unwrap(),
            LinkMsg::Heartbeat
        );
    }
}
