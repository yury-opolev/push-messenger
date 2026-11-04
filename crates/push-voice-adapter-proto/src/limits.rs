//! Every cap and timer of the protocol (design A11–A14). Peers enforce the caps on what
//! they receive; the timers are the bridge's.

use std::time::Duration;

/// Bytes of a frame's payload, either kind (the length prefix and kind byte excluded).
pub const MAX_PAYLOAD: usize = 64 * 1024;
/// Characters (Unicode scalar values) of `transcript.text` and `speak.text`.
pub const MAX_TEXT_CHARS: usize = 4_000;
/// Characters of `error.message`.
pub const MAX_ERROR_MESSAGE_CHARS: usize = 512;
/// Characters of names, versions, voices and the peer's display name.
pub const MAX_NAME_CHARS: usize = 64;
/// Bytes of an id (`call_id`, `speak_id`, `utterance_id`): printable ASCII.
pub const MAX_ID_LEN: usize = 64;
/// Bytes of a BCP 47 language tag.
pub const MAX_LANGUAGE_LEN: usize = 35;
/// Voices an adapter's hello may list.
pub const MAX_VOICES: usize = 256;
/// Versions a bridge's hello may offer.
pub const MAX_PROTOCOLS: usize = 16;

/// Each side pings this often.
pub const PING_INTERVAL: Duration = Duration::from_secs(10);
/// Nothing received for this long: the peer is dead.
pub const DEAD_PEER_AFTER: Duration = Duration::from_secs(30);
/// The adapter's hello must arrive this soon after the bridge's.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Caller audio frames the bridge queues for the adapter (1 s); beyond, the oldest is
/// dropped.
pub const TO_ADAPTER_QUEUE_FRAMES: usize = 50;
/// Speech audio frames the bridge buffers for the caller (2 s); beyond, frames are
/// dropped.
pub const TO_CALLER_BUFFER_FRAMES: usize = 100;
/// Speaks the bridge keeps unfinished at the adapter; it queues the rest itself.
pub const MAX_UNFINISHED_SPEAKS: usize = 16;
/// Control messages per second the bridge accepts from an adapter.
pub const MAX_CONTROL_PER_SECOND: u32 = 100;
/// Partial transcripts per second the bridge acts on; beyond, they are dropped.
pub const MAX_PARTIALS_PER_SECOND: u32 = 10;
