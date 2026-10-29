//! LSP-style framing: `Content-Length: N\r\n`, other headers, a blank line, then `N`
//! bytes of UTF-8 JSON.

use std::io;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// The largest frame body accepted.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// The longest header line accepted.
const MAX_HEADER_LINE: u64 = 8 * 1024;

/// Reads frames one at a time.
pub struct FrameReader<R> {
    reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
        }
    }

    /// The next frame's JSON; `None` at end of stream between frames. End of stream
    /// inside a frame, a bad header or bad JSON is an error.
    pub async fn next(&mut self) -> io::Result<Option<Value>> {
        let mut length = None;
        let mut first = true;
        loop {
            let mut line = Vec::new();
            let read = (&mut self.reader)
                .take(MAX_HEADER_LINE)
                .read_until(b'\n', &mut line)
                .await?;
            if read == 0 {
                if first {
                    return Ok(None);
                }
                return Err(truncated());
            }
            first = false;
            if line.last() != Some(&b'\n') {
                return Err(if read as u64 == MAX_HEADER_LINE {
                    invalid("frame header line too long")
                } else {
                    truncated()
                });
            }
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                break;
            }
            let line =
                std::str::from_utf8(&line).map_err(|_| invalid("frame header is not UTF-8"))?;
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| invalid("frame header without a colon"))?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                let value: usize = value
                    .trim()
                    .parse()
                    .map_err(|_| invalid("bad Content-Length"))?;
                length = Some(value);
            }
        }
        let length = length.ok_or_else(|| invalid("frame without Content-Length"))?;
        if length > MAX_FRAME {
            return Err(invalid("frame too large"));
        }
        let mut body = vec![0; length];
        self.reader.read_exact(&mut body).await?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|_| invalid("frame body is not JSON"))
    }
}

/// Writes one frame and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    writer.flush().await
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn truncated() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "stream ended inside a frame header",
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::duplex;

    use super::*;

    /// Feeds `bytes` in `chunk`-sized writes and reads frames until the first non-frame.
    async fn read_all(bytes: &[u8], chunk: usize) -> Vec<io::Result<Option<Value>>> {
        let (mut client, server) = duplex(64);
        let bytes = bytes.to_vec();
        let writer = tokio::spawn(async move {
            for piece in bytes.chunks(chunk) {
                client.write_all(piece).await.unwrap();
                client.flush().await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let mut reader = FrameReader::new(server);
        let mut results = Vec::new();
        loop {
            let result = reader.next().await;
            let done = !matches!(result, Ok(Some(_)));
            results.push(result);
            if done {
                break;
            }
        }
        writer.await.unwrap();
        results
    }

    fn frame(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[tokio::test]
    async fn reads_one_frame_then_end() {
        let results = read_all(frame(r#"{"a":1}"#).as_bytes(), 1024).await;
        assert_eq!(results[0].as_ref().unwrap(), &Some(json!({"a": 1})));
        assert!(matches!(results[1], Ok(None)));
    }

    #[tokio::test]
    async fn reads_two_frames_from_one_write() {
        let bytes = format!("{}{}", frame(r#"{"a":1}"#), frame(r#"{"b":"é"}"#));
        let results = read_all(bytes.as_bytes(), 1024).await;
        assert_eq!(results[0].as_ref().unwrap(), &Some(json!({"a": 1})));
        assert_eq!(results[1].as_ref().unwrap(), &Some(json!({"b": "é"})));
        assert!(matches!(results[2], Ok(None)));
    }

    #[tokio::test]
    async fn reads_a_frame_arriving_one_byte_at_a_time() {
        let bytes = format!("{}{}", frame(r#"{"a":[1,2,3]}"#), frame("{}"));
        let results = read_all(bytes.as_bytes(), 1).await;
        assert_eq!(results[0].as_ref().unwrap(), &Some(json!({"a": [1, 2, 3]})));
        assert_eq!(results[1].as_ref().unwrap(), &Some(json!({})));
        assert!(matches!(results[2], Ok(None)));
    }

    #[tokio::test]
    async fn ignores_other_headers_and_header_case() {
        let body = r#"{"x":true}"#;
        let bytes = format!(
            "content-type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-LENGTH:  {}\r\n\r\n{body}",
            body.len()
        );
        let results = read_all(bytes.as_bytes(), 1024).await;
        assert_eq!(results[0].as_ref().unwrap(), &Some(json!({"x": true})));
    }

    #[tokio::test]
    async fn refuses_bad_headers_and_bodies() {
        for bytes in [
            "Content-Length: nope\r\n\r\n{}".to_owned(),
            "garbage\r\n\r\n{}".to_owned(),
            "Content-Type: x\r\n\r\n{}".to_owned(),
            format!("Content-Length: {}\r\n\r\n", MAX_FRAME + 1),
            "Content-Length: 2\r\n\r\nno".to_owned(),
        ] {
            let results = read_all(bytes.as_bytes(), 1024).await;
            let error = results.last().unwrap().as_ref().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{bytes:?}");
        }
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error() {
        for bytes in [
            "Content-Length: 10\r\n\r\n{\"a\"",
            "Content-Length: 10\r\n",
            "Content-Len",
        ] {
            let results = read_all(bytes.as_bytes(), 1024).await;
            let error = results.last().unwrap().as_ref().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{bytes:?}");
        }
    }

    #[tokio::test]
    async fn written_frames_read_back() {
        let (mut client, server) = duplex(1024);
        write_frame(&mut client, &json!({"jsonrpc": "2.0", "id": 1}))
            .await
            .unwrap();
        drop(client);
        let mut reader = FrameReader::new(server);
        assert_eq!(
            reader.next().await.unwrap(),
            Some(json!({"jsonrpc": "2.0", "id": 1}))
        );
        assert_eq!(reader.next().await.unwrap(), None);
    }
}
