//! What the link says once both sides have said hello: gliff's framing (a CBOR message, then
//! the raw bytes it declares) carrying Navette's own messages. A heartbeat shows the sender
//! alive; the clipboard messages are the five of gliff's transfer engine, so its rules (chunk
//! size, ack window, serials) apply unchanged; the display's are its orders, gliff's stream
//! messages (a video frame's bytes follow it) and their feedback; the input's, home's keyboard
//! and pointer. A message this build does not know is skipped,
//! so a newer peer can add some without breaking an older one.
//!
//! To extend the link without a new [`VERSION`](crate::link::VERSION), add a top-level variant
//! to [`LinkMsg`] with a number never used before, or an optional field (`Option`, a new `#[n]`)
//! at the end of an existing one: an older peer skips the first and ignores the second. Any
//! other change is malformed to an older peer and ends the link, a new variant of a nested enum
//! (`ClipboardItem`, say) included: change the version instead.

use bytes::Bytes;
use gliff_proto::{ClientMsg, ClipboardFile, ClipboardItem, ClipboardMsg, ServerMsg};
use minicbor::{Decode, Encode};

use crate::audio;
use crate::clipboard::files::validate;
use crate::display::wire::{Display, Msg};
use crate::input;

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
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
    // The display's (`crate::display`), a block of numbers of its own. A stream's message
    // names its output, none for output 0 (so a one-output mode reads as before).
    #[n(20)]
    Display(#[n(0)] Display),
    /// Remote to home; a video frame's two streams (`data_len`, then `aux_len` bytes) follow.
    #[n(21)]
    Video(#[n(0)] ServerMsg, #[n(1)] Option<u8>),
    /// Home to remote.
    #[n(22)]
    Feedback(#[n(0)] ClientMsg, #[n(1)] Option<u8>),
    // Input (`crate::input`), home to remote, a block of numbers of its own; an event names
    // the output it is for as a stream's message does.
    #[n(30)]
    Input(#[n(0)] input::Event, #[n(1)] Option<u8>),
    /// Remote to home, once a mode: the picture runs without input.
    #[n(31)]
    NoInput {
        #[n(0)]
        reason: String,
    },
    /// Remote to home: the mode runs, but the remote leaves its lid alone (`closed_lid`).
    #[n(32)]
    NoLid {
        #[n(0)]
        reason: String,
    },
    // The remote machine's sound (`crate::audio`), a block of numbers of its own.
    #[n(48)]
    AudioStart {
        #[n(0)]
        session: u32,
    },
    #[n(49)]
    AudioStop,
    /// One Opus packet of `data_len` bytes follows the message.
    #[n(50)]
    AudioFrame {
        #[n(0)]
        session: u32,
        #[n(1)]
        seq: u32,
        #[n(2)]
        data_len: u32,
    },
    // Home's mic (`crate::audio::mic`), home to remote, a block of numbers of its own.
    #[n(51)]
    MicStart {
        #[n(0)]
        session: u32,
    },
    #[n(52)]
    MicStop,
    /// One Opus packet of `data_len` bytes follows the message.
    #[n(53)]
    MicFrame {
        #[n(0)]
        session: u32,
        #[n(1)]
        seq: u32,
        #[n(2)]
        data_len: u32,
    },
    /// The level the remote's sink and home's playback share, both ways, only when one side's
    /// mixer moves it: the linear gain of each channel, which the other side applies.
    #[n(54)]
    AudioVolume {
        #[n(0)]
        session: u32,
        #[n(1)]
        left: f32,
        #[n(2)]
        right: f32,
        #[n(3)]
        muted: bool,
    },
}

/// An output id as the link writes it: none for output 0.
pub fn output_id(output: u8) -> Option<u8> {
    (output != 0).then_some(output)
}

impl From<(u8, Msg)> for LinkMsg {
    fn from((output, msg): (u8, Msg)) -> Self {
        match msg {
            Msg::Display(display) => Self::Display(display),
            Msg::Video(msg) => Self::Video(msg, output_id(output)),
            Msg::Feedback(msg) => Self::Feedback(msg, output_id(output)),
            Msg::NoInput(reason) => Self::NoInput { reason },
            Msg::NoLid(reason) => Self::NoLid { reason },
        }
    }
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

impl From<&audio::Message> for LinkMsg {
    /// A frame's packet goes after the message, as its payload.
    fn from(msg: &audio::Message) -> Self {
        match msg {
            audio::Message::Start { session } => Self::AudioStart { session: *session },
            audio::Message::Stop => Self::AudioStop,
            audio::Message::Frame {
                session,
                seq,
                packet,
            } => Self::AudioFrame {
                session: *session,
                seq: *seq,
                data_len: packet.len() as u32,
            },
            audio::Message::Volume { session, level } => Self::AudioVolume {
                session: *session,
                left: level.left,
                right: level.right,
                muted: level.muted,
            },
            audio::Message::MicStart { session } => Self::MicStart { session: *session },
            audio::Message::MicStop => Self::MicStop,
            audio::Message::MicFrame {
                session,
                seq,
                packet,
            } => Self::MicFrame {
                session: *session,
                seq: *seq,
                data_len: packet.len() as u32,
            },
        }
    }
}

impl LinkMsg {
    /// The clipboard message this carries, or `None` for another.
    pub fn into_clipboard(self) -> Option<ClipboardMsg> {
        Some(match self {
            Self::Heartbeat
            | Self::Display(_)
            | Self::Video(..)
            | Self::Feedback(..)
            | Self::Input(..)
            | Self::NoInput { .. }
            | Self::NoLid { .. }
            | Self::AudioStart { .. }
            | Self::AudioStop
            | Self::AudioFrame { .. }
            | Self::MicStart { .. }
            | Self::MicStop
            | Self::MicFrame { .. }
            | Self::AudioVolume { .. } => return None,
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
/// types, or files [`validate`] refuses: an absolute path or one with `..`, too many entries,
/// sizes past what a count holds) made empty. The bridge then withdraws the peer's previous copy rather than keep offering it.
pub fn checked(msg: ClipboardMsg) -> ClipboardMsg {
    match msg {
        ClipboardMsg::Offer {
            serial,
            mime_types,
            files,
        } if mime_types.len() > MAX_MIME_TYPES
            || mime_types.iter().any(|m| m.len() > MAX_MIME_LEN)
            || validate(&files).is_err() =>
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

    #[test]
    fn a_stream_message_keeps_its_output_and_output_0_is_written_as_before() {
        let ping = ServerMsg::Ping { t: 3 };
        let second = LinkMsg::from((1, Msg::Video(ping.clone())));
        let bytes = minicbor::to_vec(&second).unwrap();
        assert_eq!(
            minicbor::decode::<LinkMsg>(&bytes).unwrap(),
            LinkMsg::Video(ping.clone(), Some(1))
        );
        // What a build of one output wrote, and reads.
        #[derive(Encode, Decode)]
        enum Before {
            #[n(21)]
            Video(#[n(0)] ServerMsg),
        }
        let first = minicbor::to_vec(LinkMsg::from((0, Msg::Video(ping.clone())))).unwrap();
        assert_eq!(
            first,
            minicbor::to_vec(Before::Video(ping.clone())).unwrap()
        );
        assert_eq!(
            minicbor::decode::<LinkMsg>(&first).unwrap(),
            LinkMsg::Video(ping, None)
        );
    }
}
