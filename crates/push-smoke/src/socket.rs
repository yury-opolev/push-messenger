//! The smoke test's WebSocket connections, as a client app holds them: identified with the
//! device's access token, frames sent and awaited in order. Frames that arrive while the
//! test waits for another are kept, so nothing the gateway pushes is lost to the order of
//! the waits.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, Identify, Ping, request, response};
use push_proto::{decode_frame, encode_frame};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::{CONNECT_TIMEOUT, STEP_TIMEOUT, Target, step};

/// How long a closing handshake may take; the test has passed by then.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// An identified connection.
pub(crate) struct Conn {
    name: &'static str,
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// Frames received while the test waited for something else.
    pending: VecDeque<Body>,
    next_request: u64,
}

/// What became of an `Identify`.
pub(crate) enum Identified {
    Ready(Box<Conn>, v1::Ready),
    /// The gateway closed the connection with this code.
    Closed(u16),
}

/// Connects to the gateway, through the ingress, and identifies with `token`.
pub(crate) async fn identify(
    target: &Target,
    name: &'static str,
    token: &str,
) -> anyhow::Result<Identified> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut socket = loop {
        let mut request = target.socket_url.as_str().into_client_request()?;
        if let Some(host) = &target.host {
            request
                .headers_mut()
                .insert("Host", HeaderValue::from_str(host)?);
        }
        match tokio::time::timeout(STEP_TIMEOUT, connect_async(request)).await {
            Ok(Ok((socket, _))) => break socket,
            Ok(Err(error)) if Instant::now() < deadline => {
                step(&format!("{name} cannot connect yet ({error}); retrying"));
            }
            Ok(Err(error)) => {
                return Err(error).context(format!("connect {name} to {}", target.socket_url));
            }
            Err(_) => bail!("connecting {name} timed out"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let identify = Body::Identify(Identify {
        token: token.to_owned(),
    });
    socket
        .send(Message::Binary(encode_frame(identify).into()))
        .await
        .context("send Identify")?;
    loop {
        let message = tokio::time::timeout(STEP_TIMEOUT, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("{name}: no answer to Identify within {STEP_TIMEOUT:?}"))?
            .with_context(|| format!("{name}: the connection ended before Ready"))?
            .with_context(|| format!("{name}: the connection failed before Ready"))?;
        match message {
            Message::Binary(bytes) => match decode_frame(&bytes).context("decode a frame")? {
                Body::Ready(ready) => {
                    let conn = Conn {
                        name,
                        socket,
                        pending: VecDeque::new(),
                        next_request: 0,
                    };
                    return Ok(Identified::Ready(Box::new(conn), ready));
                }
                _ => bail!("{name}: expected Ready, got another frame"),
            },
            Message::Close(frame) => {
                return Ok(Identified::Closed(
                    frame.map_or(1005, |frame| frame.code.into()),
                ));
            }
            _ => {}
        }
    }
}

impl Conn {
    pub async fn send(&mut self, body: Body) -> anyhow::Result<()> {
        self.socket
            .send(Message::Binary(encode_frame(body).into()))
            .await
            .with_context(|| format!("{}: send a frame", self.name))
    }

    /// The first frame `wanted` accepts, kept or new, within the step timeout; the others
    /// are kept. An `Error` frame that no wait accepts fails the test.
    pub async fn wait_for(
        &mut self,
        waiting_for: &str,
        wanted: impl Fn(&Body) -> bool,
    ) -> anyhow::Result<Body> {
        if let Some(index) = self.pending.iter().position(&wanted) {
            return Ok(self.pending.remove(index).expect("the index is in range"));
        }
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        loop {
            let message = tokio::time::timeout_at(deadline, self.socket.next())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("{}: no {waiting_for} within {STEP_TIMEOUT:?}", self.name)
                })?
                .with_context(|| format!("{}: the connection ended", self.name))?
                .with_context(|| format!("{}: the connection failed", self.name))?;
            let body = match message {
                Message::Binary(bytes) => decode_frame(&bytes).context("decode a frame")?,
                Message::Close(frame) => bail!(
                    "{}: closed with {:?} while waiting for {waiting_for}",
                    self.name,
                    frame.map(|frame| u16::from(frame.code))
                ),
                _ => continue,
            };
            if wanted(&body) {
                return Ok(body);
            }
            if let Body::Error(error) = &body {
                bail!(
                    "{}: an error while waiting for {waiting_for}: {:?} ({})",
                    self.name,
                    error.code(),
                    error.message
                );
            }
            self.pending.push_back(body);
        }
    }

    /// Sends a request and returns its answer: the `Response` body, or the `Error`.
    pub async fn ask(
        &mut self,
        body: request::Body,
        what: &str,
    ) -> anyhow::Result<Result<response::Body, v1::Error>> {
        self.next_request += 1;
        let request_id = self.next_request;
        let request = v1::Request {
            request_id,
            body: Some(body),
        };
        self.send(Body::Request(request)).await?;
        let answer = self
            .wait_for(what, |frame| match frame {
                Body::Response(response) => response.request_id == request_id,
                Body::Error(error) => error.request_id == Some(request_id),
                _ => false,
            })
            .await?;
        match answer {
            Body::Response(response) => Ok(Ok(response
                .body
                .with_context(|| format!("{what}: an empty answer"))?)),
            Body::Error(error) => Ok(Err(error)),
            _ => unreachable!("the wait accepts only answers"),
        }
    }

    /// Sends a request that must succeed; its answer.
    pub async fn request(
        &mut self,
        body: request::Body,
        what: &str,
    ) -> anyhow::Result<response::Body> {
        match self.ask(body, what).await? {
            Ok(answer) => Ok(answer),
            Err(error) => bail!("{what}: {:?} ({})", error.code(), error.message),
        }
    }

    /// A `Ping` answered by a `Pong`: the connection is alive and every frame sent before
    /// it was handled (the gateway handles a connection's frames in order).
    pub async fn ping(&mut self) -> anyhow::Result<()> {
        self.send(Body::Ping(Ping {})).await?;
        self.wait_for("a Pong", |frame| matches!(frame, Body::Pong(_)))
            .await?;
        Ok(())
    }

    /// Waits up to `timeout` for the gateway to close the connection; its close code.
    pub async fn closed(&mut self, waiting_for: &str, timeout: Duration) -> anyhow::Result<u16> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let message = tokio::time::timeout_at(deadline, self.socket.next())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("{}: no {waiting_for} within {timeout:?}", self.name)
                })?;
            match message {
                Some(Ok(Message::Close(frame))) => {
                    return Ok(frame.map_or(1005, |frame| frame.code.into()));
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    bail!(
                        "{}: the connection failed before {waiting_for}: {error}",
                        self.name
                    )
                }
                None => bail!("{}: the connection ended without {waiting_for}", self.name),
            }
        }
    }

    /// Closes the connection from the client's side.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.socket.close(None)).await;
    }
}
