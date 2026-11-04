//! Async frame reader and writer over tokio's `AsyncRead`/`AsyncWrite` (feature `tokio`).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::{Frame, FrameDecoder};
use crate::io::{READ_CHUNK, ReadError, WriteError, after_read};
use crate::message::{self, Message};

/// Reads frames from an async stream.
#[derive(Debug)]
pub struct AsyncFrameReader<R> {
    inner: R,
    decoder: FrameDecoder,
    chunk: Box<[u8]>,
}

impl<R: AsyncRead + Unpin> AsyncFrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            decoder: FrameDecoder::new(),
            chunk: vec![0; READ_CHUNK].into_boxed_slice(),
        }
    }

    /// The next frame; `None` when the stream ended cleanly between frames. A framing
    /// error is permanent: every later call fails too.
    ///
    /// Cancel-safe: bytes read before the future is dropped stay buffered, so it can be
    /// used in `tokio::select!` (for example against a ping timer).
    pub async fn read_frame(&mut self) -> Result<Option<Frame>, ReadError> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            let n = self.inner.read(&mut self.chunk).await?;
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

/// Writes frames to an async stream; each frame is written whole and flushed. Not
/// cancel-safe: a write dropped halfway leaves a partial frame, so the stream must be
/// closed then.
#[derive(Debug)]
pub struct AsyncFrameWriter<W> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> AsyncFrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    pub async fn write_frame(&mut self, frame: &Frame) -> Result<(), WriteError> {
        self.buf.clear();
        frame.encode_into(&mut self.buf)?;
        self.inner.write_all(&self.buf).await?;
        self.inner.flush().await?;
        Ok(())
    }

    /// Validates, encodes and writes a control message.
    pub async fn write_message<M: Message>(&mut self, message: &M) -> Result<(), WriteError> {
        let frame = message::encode(message)?;
        self.write_frame(&frame).await
    }

    /// Shuts the stream down (the peer reads a clean end).
    pub async fn shutdown(&mut self) -> Result<(), WriteError> {
        self.inner.shutdown().await?;
        Ok(())
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
    use crate::frame::{AudioFrame, FrameError, Stream};
    use crate::message::{AdapterMessage, Decoded, Pong, decode};

    #[tokio::test]
    async fn frames_cross_a_duplex_pipe() {
        let (bridge, adapter) = tokio::io::duplex(16);
        let mut writer = AsyncFrameWriter::new(bridge);
        let mut reader = AsyncFrameReader::new(adapter);
        let audio = Frame::Audio(AudioFrame {
            stream: Stream::ToCaller,
            seq: 1,
            timestamp_ms: 20,
            samples: vec![-1; 320],
        });
        let sent = audio.clone();
        let send = tokio::spawn(async move {
            writer
                .write_message(&AdapterMessage::Pong(Pong { id: 4 }))
                .await
                .unwrap();
            writer.write_frame(&sent).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let Some(Frame::Control(json)) = reader.read_frame().await.unwrap() else {
            panic!("a control frame first");
        };
        assert_eq!(
            decode::<AdapterMessage>(&json).unwrap(),
            Decoded::Known(AdapterMessage::Pong(Pong { id: 4 }))
        );
        assert_eq!(reader.read_frame().await.unwrap(), Some(audio));
        assert_eq!(reader.read_frame().await.unwrap(), None);
        send.await.unwrap();
    }

    #[tokio::test]
    async fn a_cancelled_read_loses_nothing() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = AsyncFrameReader::new(rx);
        let bytes = Frame::Control(b"{\"type\":\"x\"}".to_vec())
            .to_bytes()
            .unwrap();
        tx.write_all(&bytes[..4]).await.unwrap();
        // The read is dropped while it waits for the rest of the frame.
        let cancelled =
            tokio::time::timeout(std::time::Duration::from_millis(20), reader.read_frame()).await;
        assert!(cancelled.is_err());
        tx.write_all(&bytes[4..]).await.unwrap();
        assert_eq!(
            reader.read_frame().await.unwrap(),
            Some(Frame::Control(b"{\"type\":\"x\"}".to_vec()))
        );
    }

    #[tokio::test]
    async fn an_oversize_header_fails_without_reading_the_payload() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = AsyncFrameReader::new(rx);
        tx.write_all(&[0x7F, 0, 0, 0, 1]).await.unwrap();
        assert!(matches!(
            reader.read_frame().await,
            Err(ReadError::Frame(FrameError::TooLarge { .. }))
        ));
    }
}
