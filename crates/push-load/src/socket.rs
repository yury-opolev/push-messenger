//! The gateway's WebSocket as push-load holds it: one TCP connection per device (to the
//! target, or `--connect-to` on its port), Identify with the access token, then frames.

use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, Identify, request, response};
use push_proto::{decode_frame, encode_frame, id};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, client_async_tls};
use uuid::Uuid;

use crate::config::Target;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// No close frame: the connection dropped (RFC 6455's 1006).
pub const ABNORMAL_CLOSE: u16 = 1006;
/// A close frame without a code (RFC 6455's 1005).
pub const NO_STATUS: u16 = 1005;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    pub user: Uuid,
    pub heartbeat: Duration,
}

/// What became of an Identify.
pub enum Opened {
    Ready(Box<Socket>, Ready),
    /// The gateway closed the connection with this code before Ready.
    Closed(u16),
}

pub fn close_code(frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> u16 {
    frame.map_or(NO_STATUS, |frame| frame.code.into())
}

async fn connect(target: &Target) -> anyhow::Result<Socket> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target.dial_address()))
        .await
        .map_err(|_| anyhow::anyhow!("the TCP connection timed out"))?
        .context("the TCP connection failed")?;
    stream.set_nodelay(true).context("set TCP_NODELAY")?;
    let request = target
        .socket_url()
        .into_client_request()
        .context("the socket URL")?;
    let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, client_async_tls(request, stream))
        .await
        .map_err(|_| anyhow::anyhow!("the WebSocket handshake timed out"))?
        .context("the WebSocket handshake failed")?;
    Ok(socket)
}

/// Connects and identifies with `access_token`.
pub async fn open(target: &Target, access_token: &str) -> anyhow::Result<Opened> {
    let mut socket = connect(target).await?;
    let identify = Body::Identify(Identify {
        token: access_token.to_owned(),
    });
    send(&mut socket, identify).await?;
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no answer to Identify within {ANSWER_TIMEOUT:?}"))?;
        match message {
            Some(Ok(Message::Binary(bytes))) => {
                match decode_frame(&bytes).context("decode a frame")? {
                    Body::Ready(ready) => {
                        if ready.heartbeat_ms == 0 {
                            bail!("Ready announced no heartbeat");
                        }
                        let user = id(&ready.user_id)
                            .map_err(|_| anyhow::anyhow!("Ready names no valid user"))?;
                        let heartbeat = Duration::from_millis(u64::from(ready.heartbeat_ms));
                        return Ok(Opened::Ready(Box::new(socket), Ready { user, heartbeat }));
                    }
                    _ => bail!("the gateway answered Identify with another frame"),
                }
            }
            Some(Ok(Message::Close(frame))) => return Ok(Opened::Closed(close_code(frame))),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Ok(Opened::Closed(ABNORMAL_CLOSE)),
        }
    }
}

pub async fn send(socket: &mut Socket, body: Body) -> anyhow::Result<()> {
    tokio::time::timeout(
        WRITE_TIMEOUT,
        socket.send(Message::Binary(encode_frame(body).into())),
    )
    .await
    .map_err(|_| anyhow::anyhow!("sending a frame timed out"))?
    .context("sending a frame failed")
}

pub async fn close(mut socket: Socket) {
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
}

/// Sends a request and waits for its answer, ignoring other frames: for preparation,
/// never for the measured run.
pub async fn ask(
    socket: &mut Socket,
    request_id: u64,
    body: request::Body,
) -> anyhow::Result<Result<response::Body, v1::Error>> {
    let request = v1::Request {
        request_id,
        body: Some(body),
    };
    send(socket, Body::Request(request)).await?;
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no answer to a request within {ANSWER_TIMEOUT:?}"))?;
        match message {
            Some(Ok(Message::Binary(bytes))) => {
                match decode_frame(&bytes).context("decode a frame")? {
                    Body::Response(answer) if answer.request_id == request_id => {
                        return Ok(Ok(answer.body.context("an empty answer")?));
                    }
                    Body::Error(error) if error.request_id == Some(request_id) => {
                        return Ok(Err(error));
                    }
                    _ => {}
                }
            }
            Some(Ok(Message::Close(frame))) => {
                bail!(
                    "the gateway closed the connection with {}",
                    close_code(frame)
                );
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => bail!("the connection dropped"),
        }
    }
}
