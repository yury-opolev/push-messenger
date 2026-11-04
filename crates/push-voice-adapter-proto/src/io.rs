//! Blocking frame reader and writer over `std::io` streams (an adapter's stdin and stdout,
//! for example). See [`crate::tokio_io`] for the async ones.

use std::io::{self, Read, Write};

use crate::frame::{Frame, FrameDecoder, FrameError};
use crate::message::{self, Message, MessageError};

/// Bytes read from the stream at a time.
pub(crate) const READ_CHUNK: usize = 8 * 1024;

/// Reading a frame failed. Content-free.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("voice adapter stream: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the stream ended inside a frame ({buffered} bytes of it received)")]
    UnexpectedEof { buffered: usize },
}

/// Writing a frame failed. Content-free.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("voice adapter stream: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Message(#[from] MessageError),
}

/// What a reader does after `read` returned `n` bytes into its chunk.
pub(crate) fn after_read(
    decoder: &mut FrameDecoder,
    chunk: &[u8],
    n: usize,
) -> Result<Option<Option<Frame>>, ReadError> {
    if n == 0 {
        return match decoder.buffered() {
            0 => Ok(Some(None)),
            buffered => Err(ReadError::UnexpectedEof { buffered }),
        };
    }
    decoder.push(&chunk[..n]);
    Ok(None)
}

/// Reads frames from a blocking stream.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
    decoder: FrameDecoder,
    chunk: Box<[u8]>,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            decoder: FrameDecoder::new(),
            chunk: vec![0; READ_CHUNK].into_boxed_slice(),
        }
    }

    /// The next frame; `None` when the stream ended cleanly between frames. A framing
    /// error is permanent: every later call fails too.
    pub fn read_frame(&mut self) -> Result<Option<Frame>, ReadError> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            let n = match self.inner.read(&mut self.chunk) {
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if let Some(end) = after_read(&mut self.decoder, &self.chunk, n)? {
                return Ok(end);
            }
        }
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

/// Writes frames to a blocking stream; each frame is written whole and flushed.
#[derive(Debug)]
pub struct FrameWriter<W> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    pub fn write_frame(&mut self, frame: &Frame) -> Result<(), WriteError> {
        self.buf.clear();
        frame.encode_into(&mut self.buf)?;
        self.inner.write_all(&self.buf)?;
        self.inner.flush()?;
        Ok(())
    }

    /// Validates, encodes and writes a control message.
    pub fn write_message<M: Message>(&mut self, message: &M) -> Result<(), WriteError> {
        let frame = message::encode(message)?;
        self.write_frame(&frame)
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{AudioFrame, Stream};
    use crate::message::{BridgeMessage, Ping};

    /// A reader that returns at most `step` bytes per read.
    struct Trickle {
        bytes: Vec<u8>,
        at: usize,
        step: usize,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.bytes.len() - self.at);
            buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    #[test]
    fn written_frames_read_back_through_short_reads() {
        let mut writer = FrameWriter::new(Vec::new());
        writer
            .write_message(&BridgeMessage::Ping(Ping { id: 9 }))
            .unwrap();
        let audio = Frame::Audio(AudioFrame {
            stream: Stream::FromCaller,
            seq: 0,
            timestamp_ms: 0,
            samples: vec![3; 320],
        });
        writer.write_frame(&audio).unwrap();
        let bytes = writer.into_inner();
        for step in [1, 3, 7, 4096] {
            let mut reader = FrameReader::new(Trickle {
                bytes: bytes.clone(),
                at: 0,
                step,
            });
            assert_eq!(
                reader.read_frame().unwrap(),
                Some(Frame::Control(br#"{"type":"ping","id":9}"#.to_vec()))
            );
            assert_eq!(reader.read_frame().unwrap(), Some(audio.clone()));
            assert_eq!(reader.read_frame().unwrap(), None);
        }
    }

    #[test]
    fn a_stream_ending_inside_a_frame_is_an_error() {
        let bytes = Frame::Control(b"{}".to_vec()).to_bytes().unwrap();
        let mut reader = FrameReader::new(&bytes[..bytes.len() - 1]);
        assert!(matches!(
            reader.read_frame(),
            Err(ReadError::UnexpectedEof { buffered: 6 })
        ));
    }

    #[test]
    fn an_invalid_message_is_not_written() {
        let mut writer = FrameWriter::new(Vec::new());
        let hello = crate::message::BridgeHello::v1("1.0", "not a tag!");
        assert!(matches!(
            writer.write_message(&BridgeMessage::Hello(hello)),
            Err(WriteError::Message(_))
        ));
        assert!(writer.get_ref().is_empty());
    }
}
