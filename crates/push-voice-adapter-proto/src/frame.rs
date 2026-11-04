//! Frames: `len: u32 BE` (payload bytes), `kind: u8`, `payload[len]`.
//!
//! Kind 1 is a JSON control message, kind 2 audio: a 9-byte header `stream: u8`,
//! `seq: u32 BE`, `timestamp_ms: u32 BE`, then signed 16-bit little-endian samples. Either
//! payload holds at most [`MAX_PAYLOAD`] bytes. Any other kind or stream, an oversize
//! length, an empty control payload or a malformed audio payload is a [`FrameError`]; the
//! decoder refuses an oversize length from the header alone, before buffering the payload.

use std::fmt;

use crate::limits::MAX_PAYLOAD;

/// Bytes before a frame's payload: the length and the kind.
pub const HEADER_LEN: usize = 5;
/// Bytes of an audio payload's header: stream, seq, timestamp.
pub const AUDIO_HEADER_LEN: usize = 9;
/// Samples an audio frame may carry at most.
pub const MAX_SAMPLES: usize = (MAX_PAYLOAD - AUDIO_HEADER_LEN) / 2;

/// What a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A JSON control message.
    Control = 1,
    /// Audio.
    Audio = 2,
}

impl Kind {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Kind::Control),
            2 => Some(Kind::Audio),
            _ => None,
        }
    }
}

/// Which way audio flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Stream {
    /// The caller's audio, bridge → adapter.
    FromCaller = 0,
    /// Speech for the caller, adapter → bridge.
    ToCaller = 1,
}

impl Stream {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Stream::FromCaller),
            1 => Some(Stream::ToCaller),
            _ => None,
        }
    }
}

/// One audio frame. `Debug` shows the sample count, never the samples.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub stream: Stream,
    /// Per stream and call: 0, 1, 2, … (wrapping); a gap means dropped frames.
    pub seq: u32,
    /// Stream 0: ms since the call's first frame. Stream 1: ms since the current speak's
    /// first frame.
    pub timestamp_ms: u32,
    pub samples: Vec<i16>,
}

impl fmt::Debug for AudioFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioFrame")
            .field("stream", &self.stream)
            .field("seq", &self.seq)
            .field("timestamp_ms", &self.timestamp_ms)
            .field("samples", &self.samples.len())
            .finish()
    }
}

/// One frame. `Debug` shows a control payload's length, never its bytes.
#[derive(Clone, PartialEq, Eq)]
pub enum Frame {
    /// A control message's JSON (see [`crate::message`]).
    Control(Vec<u8>),
    Audio(AudioFrame),
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Frame::Control(json) => write!(f, "Control({} bytes)", json.len()),
            Frame::Audio(audio) => f.debug_tuple("Audio").field(audio).finish(),
        }
    }
}

/// A frame that cannot be decoded or encoded. Content-free: it never holds payload bytes.
/// Every one is fatal for the connection when decoding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame payload of {len} bytes exceeds the limit of {max} bytes")]
    TooLarge { len: u64, max: usize },
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("empty control frame")]
    EmptyControl,
    #[error(
        "malformed audio payload of {len} bytes (a 9-byte header and at least one 16-bit sample)"
    )]
    BadAudio { len: usize },
    #[error("unknown audio stream {0}")]
    UnknownStream(u8),
}

impl Frame {
    pub fn kind(&self) -> Kind {
        match self {
            Frame::Control(_) => Kind::Control,
            Frame::Audio(_) => Kind::Audio,
        }
    }

    /// Appends the encoded frame to `out` (nothing on error).
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        match self {
            Frame::Control(json) => {
                if json.is_empty() {
                    return Err(FrameError::EmptyControl);
                }
                if json.len() > MAX_PAYLOAD {
                    return Err(too_large(json.len()));
                }
                out.reserve(HEADER_LEN + json.len());
                out.extend_from_slice(&(json.len() as u32).to_be_bytes());
                out.push(Kind::Control as u8);
                out.extend_from_slice(json);
            }
            Frame::Audio(audio) => {
                if audio.samples.is_empty() {
                    return Err(FrameError::BadAudio {
                        len: AUDIO_HEADER_LEN,
                    });
                }
                let len = AUDIO_HEADER_LEN + audio.samples.len() * 2;
                if len > MAX_PAYLOAD {
                    return Err(too_large(len));
                }
                out.reserve(HEADER_LEN + len);
                out.extend_from_slice(&(len as u32).to_be_bytes());
                out.push(Kind::Audio as u8);
                out.push(audio.stream as u8);
                out.extend_from_slice(&audio.seq.to_be_bytes());
                out.extend_from_slice(&audio.timestamp_ms.to_be_bytes());
                for sample in &audio.samples {
                    out.extend_from_slice(&sample.to_le_bytes());
                }
            }
        }
        Ok(())
    }

    /// The encoded frame.
    pub fn to_bytes(&self) -> Result<Vec<u8>, FrameError> {
        let mut out = Vec::new();
        self.encode_into(&mut out)?;
        Ok(out)
    }
}

fn too_large(len: usize) -> FrameError {
    FrameError::TooLarge {
        len: len as u64,
        max: MAX_PAYLOAD,
    }
}

/// Checks a frame header: the payload length and kind.
fn check_header(len: u32, kind: u8) -> Result<Kind, FrameError> {
    if len as usize > MAX_PAYLOAD {
        return Err(FrameError::TooLarge {
            len: u64::from(len),
            max: MAX_PAYLOAD,
        });
    }
    let kind = Kind::from_u8(kind).ok_or(FrameError::UnknownKind(kind))?;
    if kind == Kind::Control && len == 0 {
        return Err(FrameError::EmptyControl);
    }
    if kind == Kind::Audio && (len as usize) < AUDIO_HEADER_LEN + 2 {
        return Err(FrameError::BadAudio { len: len as usize });
    }
    Ok(kind)
}

/// Decodes one frame's payload of `kind`; the length was checked.
fn decode_payload(kind: Kind, payload: &[u8]) -> Result<Frame, FrameError> {
    match kind {
        Kind::Control => Ok(Frame::Control(payload.to_vec())),
        Kind::Audio => {
            let len = payload.len();
            if len < AUDIO_HEADER_LEN + 2 || (len - AUDIO_HEADER_LEN) % 2 != 0 {
                return Err(FrameError::BadAudio { len });
            }
            let stream =
                Stream::from_u8(payload[0]).ok_or(FrameError::UnknownStream(payload[0]))?;
            let seq = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
            let timestamp_ms = u32::from_be_bytes([payload[5], payload[6], payload[7], payload[8]]);
            let samples = payload[AUDIO_HEADER_LEN..]
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            Ok(Frame::Audio(AudioFrame {
                stream,
                seq,
                timestamp_ms,
                samples,
            }))
        }
    }
}

/// An incremental, sans-IO frame decoder: [`push`](Self::push) bytes as they arrive (split
/// or merged anyhow), then take frames with [`next_frame`](Self::next_frame) until it
/// returns `None`. A header is checked as soon as its 5 bytes are in, so an oversize
/// length is refused before its payload is buffered. After an error the decoder returns
/// that error forever: framing errors are fatal.
///
/// Pushing only what a bounded read returned, and taking every frame before the next
/// push, keeps the buffer under `HEADER_LEN + MAX_PAYLOAD` plus one read.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    failed: Option<FrameError>,
}

impl fmt::Debug for FrameDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDecoder")
            .field("buffered", &self.buf.len())
            .field("failed", &self.failed)
            .finish()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds received bytes (ignored after an error).
    pub fn push(&mut self, bytes: &[u8]) {
        if self.failed.is_none() {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// Bytes received but not yet decoded: non-zero at the end of the stream means it
    /// ended inside a frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next complete frame, `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        let decoded = check_header(len, self.buf[4]).and_then(|kind| {
            let end = HEADER_LEN + len as usize;
            if self.buf.len() < end {
                return Ok(None);
            }
            let frame = decode_payload(kind, &self.buf[HEADER_LEN..end])?;
            self.buf.drain(..end);
            Ok(Some(frame))
        });
        if let Err(error) = &decoded {
            self.failed = Some(error.clone());
            self.buf = Vec::new();
        }
        decoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(stream: Stream, seq: u32, samples: Vec<i16>) -> Frame {
        Frame::Audio(AudioFrame {
            stream,
            seq,
            timestamp_ms: seq.wrapping_mul(20),
            samples,
        })
    }

    fn decode_all(bytes: &[u8]) -> Result<Vec<Frame>, FrameError> {
        let mut decoder = FrameDecoder::new();
        decoder.push(bytes);
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame()? {
            frames.push(frame);
        }
        assert_eq!(decoder.buffered(), 0, "trailing bytes");
        Ok(frames)
    }

    #[test]
    fn a_control_frame_has_the_documented_layout() {
        let bytes = Frame::Control(br#"{"type":"ping","id":7}"#.to_vec())
            .to_bytes()
            .unwrap();
        assert_eq!(&bytes[..5], &[0, 0, 0, 22, 1]);
        assert_eq!(&bytes[5..], br#"{"type":"ping","id":7}"#);
    }

    #[test]
    fn an_audio_frame_has_the_documented_layout() {
        let frame = Frame::Audio(AudioFrame {
            stream: Stream::ToCaller,
            seq: 0x0102_0304,
            timestamp_ms: 0x0A0B_0C0D,
            samples: vec![1, -2],
        });
        let bytes = frame.to_bytes().unwrap();
        assert_eq!(
            bytes,
            vec![
                0, 0, 0, 13, 2, // len 9 + 4, kind
                1, 1, 2, 3, 4, 0x0A, 0x0B, 0x0C, 0x0D, // stream, seq, timestamp
                1, 0, 0xFE, 0xFF, // 1, -2 little-endian
            ]
        );
        assert_eq!(decode_all(&bytes).unwrap(), vec![frame]);
    }

    #[test]
    fn frames_round_trip_and_merge() {
        let frames = vec![
            Frame::Control(b"{}".to_vec()),
            audio(Stream::FromCaller, 0, vec![0; 320]),
            audio(Stream::ToCaller, u32::MAX, vec![i16::MIN, i16::MAX]),
            Frame::Control(vec![b'x'; MAX_PAYLOAD]),
            audio(Stream::FromCaller, 1, vec![7; MAX_SAMPLES]),
        ];
        let mut bytes = Vec::new();
        for frame in &frames {
            frame.encode_into(&mut bytes).unwrap();
        }
        assert_eq!(decode_all(&bytes).unwrap(), frames);
    }

    #[test]
    fn a_frame_split_byte_by_byte_decodes_once_complete() {
        let bytes = audio(Stream::FromCaller, 3, vec![5; 320])
            .to_bytes()
            .unwrap();
        let mut decoder = FrameDecoder::new();
        for (i, byte) in bytes.iter().enumerate() {
            assert_eq!(decoder.next_frame().unwrap(), None, "at {i}");
            decoder.push(&[*byte]);
        }
        assert!(decoder.next_frame().unwrap().is_some());
        assert_eq!(decoder.next_frame().unwrap(), None);
    }

    #[test]
    fn an_oversize_length_is_refused_from_the_header_alone() {
        let mut decoder = FrameDecoder::new();
        decoder.push(&[0, 1, 0, 1, 1]);
        assert_eq!(
            decoder.next_frame(),
            Err(FrameError::TooLarge {
                len: 65_537,
                max: MAX_PAYLOAD
            })
        );
        assert_eq!(decoder.buffered(), 0);
        decoder.push(&[0; 100]);
        assert_eq!(decoder.buffered(), 0, "nothing is buffered after an error");
        assert!(decoder.next_frame().is_err(), "the error sticks");
        let mut decoder = FrameDecoder::new();
        decoder.push(&[0xFF, 0xFF, 0xFF, 0xFF, 2]);
        assert!(matches!(
            decoder.next_frame(),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn the_limit_itself_is_accepted() {
        let bytes = Frame::Control(vec![b' '; MAX_PAYLOAD]).to_bytes().unwrap();
        assert_eq!(decode_all(&bytes).unwrap().len(), 1);
        assert_eq!(
            Frame::Control(vec![b' '; MAX_PAYLOAD + 1]).to_bytes(),
            Err(FrameError::TooLarge {
                len: MAX_PAYLOAD as u64 + 1,
                max: MAX_PAYLOAD
            })
        );
        assert!(
            audio(Stream::ToCaller, 0, vec![0; MAX_SAMPLES + 1])
                .to_bytes()
                .is_err()
        );
    }

    #[test]
    fn bad_kinds_streams_and_payloads_are_errors() {
        for kind in [0u8, 3, 255] {
            assert_eq!(
                decode_all(&[0, 0, 0, 1, kind, b'x']),
                Err(FrameError::UnknownKind(kind))
            );
        }
        assert_eq!(decode_all(&[0, 0, 0, 0, 1]), Err(FrameError::EmptyControl));
        // An audio payload: too short, odd and an unknown stream.
        assert_eq!(
            decode_all(&[0, 0, 0, 9, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(FrameError::BadAudio { len: 9 })
        );
        assert_eq!(
            decode_all(&[0, 0, 0, 12, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3]),
            Err(FrameError::BadAudio { len: 12 })
        );
        assert_eq!(
            decode_all(&[0, 0, 0, 11, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2]),
            Err(FrameError::UnknownStream(2))
        );
        assert_eq!(
            Frame::Control(Vec::new()).to_bytes(),
            Err(FrameError::EmptyControl)
        );
        assert!(audio(Stream::ToCaller, 0, Vec::new()).to_bytes().is_err());
    }

    #[test]
    fn debug_never_shows_payloads() {
        let control = format!("{:?}", Frame::Control(b"secret words".to_vec()));
        assert_eq!(control, "Control(12 bytes)");
        let audio = format!("{:?}", audio(Stream::FromCaller, 1, vec![12345; 320]));
        assert!(!audio.contains("12345"), "{audio}");
        assert!(audio.contains("samples: 320"), "{audio}");
    }
}
