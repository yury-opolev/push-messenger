//! The running bridge's control channel from the command line (`pause`, `resume`): one
//! connection, one request at a time, events ignored.

use std::path::Path;
use std::time::Duration;

use push_bridge_control::transport::{self, Stream};
use push_bridge_control::{
    Call, ErrorBody, Event, Request, ServerLine, Status, encode, endpoint, instance_id,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// How long the bridge may take to greet and to answer `status`.
const QUICK: Duration = Duration::from_secs(10);

/// Why the bridge could not be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    /// No bridge serves the folder's control channel.
    NotRunning,
    /// The bridge did not answer in time.
    Timeout,
    /// The connection broke or carried something unexpected.
    Broken(String),
}

/// A connection to a running bridge.
pub struct Remote<S> {
    stream: BufReader<S>,
    next_id: u64,
}

/// Connects to the bridge that runs for `dir`.
pub async fn connect(dir: &Path) -> Result<Remote<Stream>, RemoteError> {
    let endpoint = endpoint(&instance_id(dir)).ok_or(RemoteError::NotRunning)?;
    let stream = transport::connect(&endpoint)
        .await
        .map_err(|_| RemoteError::NotRunning)?;
    Remote::open(stream).await
}

impl<S: AsyncRead + AsyncWrite + Unpin> Remote<S> {
    /// Takes over a connected stream and reads the bridge's greeting.
    pub async fn open(stream: S) -> Result<Self, RemoteError> {
        let mut remote = Self {
            stream: BufReader::new(stream),
            next_id: 1,
        };
        let line = tokio::time::timeout(QUICK, remote.line())
            .await
            .map_err(|_| RemoteError::Timeout)??;
        match serde_json::from_str::<ServerLine>(&line) {
            Ok(ServerLine::Event(event)) if matches!(event.event, Event::Hello { .. }) => {
                Ok(remote)
            }
            _ => Err(RemoteError::Broken(
                "the bridge did not say hello".to_owned(),
            )),
        }
    }

    async fn line(&mut self) -> Result<String, RemoteError> {
        let mut line = String::new();
        match self.stream.read_line(&mut line).await {
            Ok(0) => Err(RemoteError::Broken(
                "the bridge closed the connection".to_owned(),
            )),
            Ok(_) => Ok(line),
            Err(error) => Err(RemoteError::Broken(error.to_string())),
        }
    }

    /// Sends `call` and waits up to `limit` for its response (events are skipped).
    pub async fn call(
        &mut self,
        call: Call,
        limit: Duration,
    ) -> Result<Result<Value, ErrorBody>, RemoteError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = encode(&Request { id, call });
        let sent = async {
            self.stream.write_all(request.as_bytes()).await?;
            self.stream.flush().await
        };
        sent.await
            .map_err(|error| RemoteError::Broken(error.to_string()))?;
        let answer = async {
            loop {
                let line = self.line().await?;
                if let Ok(ServerLine::Response(response)) = serde_json::from_str(&line) {
                    if response.id != id {
                        continue;
                    }
                    return Ok(if response.ok {
                        Ok(response.result.unwrap_or(Value::Null))
                    } else {
                        Err(response.error.unwrap_or_else(|| ErrorBody {
                            code: "unknown".to_owned(),
                            message: "the bridge gave no reason".to_owned(),
                        }))
                    });
                }
            }
        };
        tokio::time::timeout(limit, answer)
            .await
            .map_err(|_| RemoteError::Timeout)?
    }

    /// The backend's name from the bridge's status, such as "Coda".
    pub async fn backend_name(&mut self) -> Option<String> {
        let status = self.call(Call::Status, QUICK).await.ok()?.ok()?;
        serde_json::from_value::<Status>(status)
            .ok()?
            .backend
            .map(|backend| backend.name)
    }
}

#[cfg(test)]
mod tests {
    use push_bridge::control::{Action, ControlHub};
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn asks_the_bridge_and_skips_its_events() {
        let hub = ControlHub::new("0011223344556677".into());
        let (client, server) = tokio::io::duplex(64 * 1024);
        let _served = hub.serve_stream(server);
        let calls = hub.calls();
        tokio::spawn(async move {
            let mut calls = calls.lock().await;
            while let Some(call) = calls.recv().await {
                assert_eq!(call.action, Action::Resume);
                let _ = call.reply.send(Ok(json!({"resumed": true})));
            }
        });
        let mut remote = Remote::open(client).await.unwrap();
        assert_eq!(remote.backend_name().await, None);
        assert_eq!(
            remote.call(Call::ResumeBackend, QUICK).await,
            Ok(Ok(json!({"resumed": true})))
        );
    }
}
