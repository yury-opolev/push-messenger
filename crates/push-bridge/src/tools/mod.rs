//! Coda tools (spec 2026-11-09 proactive agents §8): the bridge's actions served to Coda as
//! MCP tools, without changing Coda.
//!
//! - [`endpoint`]: the bridge listens on a private **tools endpoint** (the control
//!   channel's same-user transport, named `push-agent-bridge-<id>-tools`) and runs each
//!   action a client sends with the per-run token, as the backend's own action.
//! - [`mcp`]: `push-agent-bridge tools-mcp`, a small stdio MCP server Coda starts (from the
//!   `.mcp.json` the Coda backend writes) that forwards each tool call to the endpoint.
//!
//! The line protocol on the endpoint, one JSON object per line (at most
//! [`push_bridge_control::MAX_LINE`] bytes), any number of requests per connection:
//! the client sends `{"token": "<hex>", "action": <Action>}`; the bridge answers
//! `{"result": <ActionResult>}` or `{"error": "unauthorized" | "invalid"}`. A wrong token
//! closes the connection.

pub mod endpoint;
pub mod mcp;

use std::io;
use std::path::PathBuf;

use push_bridge_control::Endpoint;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

use crate::actions::ActionResult;

/// The tools endpoint, for `tools-mcp`: a pipe name (Windows) or a socket path (Unix).
pub const ENDPOINT_ENV: &str = "PUSH_BRIDGE_TOOLS";
/// The per-run token, for `tools-mcp`.
pub const TOKEN_ENV: &str = "PUSH_BRIDGE_TOOLS_TOKEN";
/// Appended to the instance id: the control channel's name, `-tools`.
const SUFFIX: &str = "-tools";

/// The tools endpoint of the bridge with instance id `instance`: the control channel's
/// with `-tools` appended (`\\.\pipe\push-agent-bridge-<id>-tools`,
/// `$XDG_RUNTIME_DIR/push-agent-bridge/<id>-tools.sock`); `None` on Unix without a usable
/// `XDG_RUNTIME_DIR`.
pub fn tools_endpoint(instance: &str) -> Option<Endpoint> {
    push_bridge_control::endpoint(&format!("{instance}{SUFFIX}"))
}

/// The endpoint as `tools-mcp` gets it in [`ENDPOINT_ENV`].
pub fn endpoint_text(endpoint: &Endpoint) -> String {
    match endpoint {
        Endpoint::Pipe(name) => name.clone(),
        Endpoint::Socket(path) => path.to_string_lossy().into_owned(),
    }
}

/// The endpoint named by [`endpoint_text`]: a pipe on Windows, a socket elsewhere.
pub fn parse_endpoint(text: &str) -> Option<Endpoint> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if cfg!(windows) {
        Some(Endpoint::Pipe(text.to_owned()))
    } else {
        Some(Endpoint::Socket(PathBuf::from(text)))
    }
}

/// A fresh random 256-bit token, as 64 lowercase hex characters.
pub fn new_token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Why the endpoint did not run an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The token is wrong: the connection is closed.
    Unauthorized,
    /// Not a request, or not an action.
    Invalid,
}

/// The endpoint's answer to one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Result(ActionResult),
    Error(Refusal),
}

/// One line read by [`next_line`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Line {
    Text(String),
    /// Longer than allowed: the reader stops there.
    TooLong,
}

/// The next non-blank line (without its `\r\n`), at most `max` bytes; `None` at the end.
pub(crate) async fn next_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> io::Result<Option<Line>> {
    let limit = u64::try_from(max).unwrap_or(u64::MAX) + 1;
    loop {
        let mut buffer = Vec::new();
        let read = (&mut *reader)
            .take(limit)
            .read_until(b'\n', &mut buffer)
            .await?;
        if read == 0 {
            return Ok(None);
        }
        let complete = buffer.last() == Some(&b'\n');
        if complete {
            buffer.pop();
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
        }
        if buffer.len() > max {
            return Ok(Some(Line::TooLong));
        }
        if !buffer.iter().all(u8::is_ascii_whitespace) {
            return Ok(Some(Line::Text(
                String::from_utf8_lossy(&buffer).into_owned(),
            )));
        }
        if !complete {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionCode;

    #[test]
    fn the_endpoint_is_the_control_channel_s_with_a_suffix() {
        let id = "00112233aabbccdd";
        #[cfg(windows)]
        assert_eq!(
            tools_endpoint(id),
            Some(Endpoint::Pipe(
                r"\\.\pipe\push-agent-bridge-00112233aabbccdd-tools".into()
            ))
        );
        let endpoint = push_bridge_control::endpoint_for(
            &format!("{id}{SUFFIX}"),
            Some(std::ffi::OsStr::new("/run/user/1000")),
        );
        #[cfg(unix)]
        assert_eq!(
            endpoint,
            Some(Endpoint::Socket(PathBuf::from(
                "/run/user/1000/push-agent-bridge/00112233aabbccdd-tools.sock"
            )))
        );
        let endpoint = endpoint.unwrap();
        assert_eq!(parse_endpoint(&endpoint_text(&endpoint)), Some(endpoint));
        assert_eq!(parse_endpoint("  "), None);
    }

    #[test]
    fn tokens_are_256_random_bits_in_hex() {
        let (one, two) = (new_token(), new_token());
        assert_eq!(one.len(), 64);
        assert!(one.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(one, two);
    }

    #[test]
    fn replies_read_as_the_protocol_says() {
        assert_eq!(
            serde_json::to_value(Reply::Error(Refusal::Unauthorized)).unwrap(),
            serde_json::json!({"error": "unauthorized"})
        );
        assert_eq!(
            serde_json::to_value(Reply::Result(ActionResult::new(ActionCode::Paused, "p")))
                .unwrap(),
            serde_json::json!({"result": {"code": "paused", "detail": "p"}})
        );
    }

    #[tokio::test]
    async fn lines_skip_blanks_and_stop_when_too_long() {
        let input = b"\r\n{\"a\":1}\r\n  \nlast".to_vec();
        let mut reader = tokio::io::BufReader::new(&input[..]);
        assert_eq!(
            next_line(&mut reader, 16).await.unwrap(),
            Some(Line::Text("{\"a\":1}".into()))
        );
        assert_eq!(
            next_line(&mut reader, 16).await.unwrap(),
            Some(Line::Text("last".into()))
        );
        assert_eq!(next_line(&mut reader, 16).await.unwrap(), None);
        let long = format!("{}\nnext\n", "x".repeat(17));
        let mut reader = tokio::io::BufReader::new(long.as_bytes());
        assert_eq!(
            next_line(&mut reader, 16).await.unwrap(),
            Some(Line::TooLong)
        );
    }
}
