//! push-smoke: the end-to-end smoke test of a running push-messenger.
//!
//! Against a deployment it:
//!
//! 1. creates Alice and Bob, one device each, as accepted contacts with a direct
//!    conversation, through message-svc's DEV ONLY `DevSetup` API;
//! 2. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify`, and
//!    waits for `Ready`;
//! 3. sends a message from Alice and waits for her `SendAck`;
//! 4. waits for Bob's `Deliver` of that message;
//! 5. acks it from Bob, then pings: the `Pong` proves the ack went through, since the
//!    gateway handles a connection's frames in order and answers a failed ack with an
//!    `Error`.
//!
//! The deployment must run message-svc with `PUSH_DEV_SETUP=1` and the gateway with
//! `PUSH_DEV_AUTH=1`.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::{
    MakeContactsRequest, OpenDirectRequest, RegisterDeviceRequest, RegisterUserRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Send};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

/// How long each step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep trying to connect: an ingress may answer 503 for a few seconds after
/// its backends become ready.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where the deployment is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// gRPC URL of message-svc, for the dev setup calls: `http://host:50051`.
    pub setup_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header for the WebSocket request, when the URL names an ingress controller
    /// rather than the virtual host it routes (`push.local`).
    pub host: Option<String>,
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --setup <grpc-url> --socket <ws-url> [--host <host>]";

impl Target {
    /// Reads `--setup`, `--socket` and `--host` from `args`; each one missing there is read
    /// from `env` (`PUSH_SMOKE_SETUP_URL`, `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_HOST`).
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (mut setup_url, mut socket_url, mut host) = (None, None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--setup" => &mut setup_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
                other => return Err(format!("unknown argument {other:?}; {USAGE}")),
            };
            *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        }
        let setup_url = setup_url
            .or_else(|| env("PUSH_SMOKE_SETUP_URL"))
            .ok_or_else(|| format!("--setup is required; {USAGE}"))?;
        let socket_url = socket_url
            .or_else(|| env("PUSH_SMOKE_SOCKET_URL"))
            .ok_or_else(|| format!("--socket is required; {USAGE}"))?;
        let host = host
            .or_else(|| env("PUSH_SMOKE_HOST"))
            .filter(|host| !host.is_empty());
        Ok(Self {
            setup_url,
            socket_url,
            host,
        })
    }
}

/// One user with one device.
struct Client {
    name: &'static str,
    user: Uuid,
    device: Uuid,
}

/// Runs the smoke test, printing each step. Fails at the first step that does not
/// complete within 10 s.
pub async fn run(target: &Target) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut setup = DevSetupClient::connect(target.setup_url.clone())
        .await
        .with_context(|| format!("connect to message-svc at {}", target.setup_url))?;
    let alice = Client::register("alice", &mut setup).await?;
    let bob = Client::register("bob", &mut setup).await?;
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice.user),
            peer_id: id_bytes(bob.user),
        })
        .await
        .context("make Alice and Bob contacts")?;
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice.user),
            peer_id: id_bytes(bob.user),
        })
        .await
        .context("open their direct conversation")?
        .into_inner()
        .conversation_id;
    step(&format!(
        "set up alice={} bob={} as contacts with a direct conversation",
        alice.user, bob.user
    ));

    let mut bob_socket = bob.connect(target).await?;
    let mut alice_socket = alice.connect(target).await?;

    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
    let sent_at = Instant::now();
    send(
        &mut alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.clone(),
            content_type: ContentType::Text.into(),
            body: text.clone().into_bytes(),
            client_ts: None,
        }),
    )
    .await?;
    let ack = loop {
        match next(&mut alice_socket, "alice's SendAck").await? {
            Body::SendAck(ack) if ack.client_msg_id == client_msg_id => break ack,
            Body::Error(error) => bail!("alice's send failed: {error:?}"),
            // Alice's own devices also receive her message.
            _ => {}
        }
    };
    step(&format!(
        "alice got SendAck seq={} after {} ms",
        ack.seq,
        sent_at.elapsed().as_millis()
    ));

    let entry = loop {
        match next(&mut bob_socket, "bob's Deliver").await? {
            Body::Deliver(entry)
                if entry.seq == ack.seq && entry.conversation_id == conversation =>
            {
                break entry;
            }
            Body::Error(error) => bail!("bob got an error: {error:?}"),
            _ => {}
        }
    };
    if entry.body.as_deref() != Some(text.as_bytes()) || entry.sender_id != id_bytes(alice.user) {
        bail!("bob received the wrong message: {entry:?}");
    }
    step(&format!(
        "bob got Deliver seq={} after {} ms",
        entry.seq,
        sent_at.elapsed().as_millis()
    ));

    send(
        &mut bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation,
            seq: entry.seq,
        }),
    )
    .await?;
    send(&mut bob_socket, Body::Ping(Ping {})).await?;
    loop {
        match next(&mut bob_socket, "bob's Pong").await? {
            Body::Pong(_) => break,
            Body::Error(error) => bail!("bob's ack failed: {error:?}"),
            _ => {}
        }
    }
    step("bob acked; the gateway answered the following Ping");

    for socket in [&mut alice_socket, &mut bob_socket] {
        let _ = socket.close(None).await;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

impl Client {
    /// Registers a new user with one device.
    async fn register(
        name: &'static str,
        setup: &mut DevSetupClient<tonic::transport::Channel>,
    ) -> anyhow::Result<Self> {
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        setup
            .register_user(RegisterUserRequest {
                user_id: id_bytes(user),
            })
            .await
            .with_context(|| format!("register {name}"))?;
        setup
            .register_device(RegisterDeviceRequest {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
            })
            .await
            .with_context(|| format!("register {name}'s device"))?;
        Ok(Self { name, user, device })
    }

    /// Connects and identifies; returns once `Ready` arrived.
    async fn connect(&self, target: &Target) -> anyhow::Result<Socket> {
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
                    step(&format!(
                        "{} cannot connect yet ({error}); retrying",
                        self.name
                    ));
                }
                Ok(Err(error)) => {
                    return Err(error)
                        .context(format!("connect {} to {}", self.name, target.socket_url));
                }
                Err(_) => bail!("connecting {} timed out", self.name),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        send(
            &mut socket,
            Body::Identify(Identify {
                token: String::new(),
                dev_user_id: id_bytes(self.user),
                dev_device_id: id_bytes(self.device),
            }),
        )
        .await?;
        match next(&mut socket, "Ready").await? {
            Body::Ready(ready) => {
                step(&format!(
                    "{} connected (Ready, heartbeat {} ms)",
                    self.name, ready.heartbeat_ms
                ));
                Ok(socket)
            }
            other => bail!("{} expected Ready, got {other:?}", self.name),
        }
    }
}

async fn send(socket: &mut Socket, body: Body) -> anyhow::Result<()> {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .context("send a frame")
}

/// The next frame, within the step timeout.
async fn next(socket: &mut Socket, waiting_for: &str) -> anyhow::Result<Body> {
    loop {
        let message = tokio::time::timeout(STEP_TIMEOUT, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no {waiting_for} within {STEP_TIMEOUT:?}"))?
            .with_context(|| format!("the connection ended while waiting for {waiting_for}"))?
            .with_context(|| format!("the connection failed while waiting for {waiting_for}"))?;
        match message {
            Message::Binary(bytes) => return decode_frame(&bytes).context("decode a frame"),
            Message::Close(frame) => bail!("closed while waiting for {waiting_for}: {frame:?}"),
            _ => {}
        }
    }
}

fn step(message: &str) {
    println!("smoke: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn flags_name_the_target() {
        let target = Target::parse(
            args(&[
                "--setup",
                "http://svc:50051",
                "--socket",
                "ws://ingress/v1/socket",
                "--host",
                "push.local",
            ]),
            |_| None,
        );
        assert_eq!(
            target,
            Ok(Target {
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
            })
        );
    }

    #[test]
    fn missing_flags_come_from_the_environment() {
        let target = Target::parse(args(&["--socket", "ws://gw/v1/socket"]), |name| {
            (name == "PUSH_SMOKE_SETUP_URL").then(|| "http://svc:50051".to_owned())
        });
        assert_eq!(
            target,
            Ok(Target {
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://gw/v1/socket".to_owned(),
                host: None,
            })
        );
    }

    #[test]
    fn a_missing_url_or_unknown_flag_is_refused() {
        assert!(Target::parse(args(&["--setup", "http://svc:50051"]), |_| None).is_err());
        assert!(Target::parse(args(&["--verbose"]), |_| None).is_err());
        assert!(Target::parse(args(&["--setup"]), |_| None).is_err());
    }
}
