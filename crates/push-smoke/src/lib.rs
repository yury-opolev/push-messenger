//! push-smoke: the end-to-end smoke test of a running push-messenger.
//!
//! Against a deployment it:
//!
//! 1. enrolls Alice and Bob through auth-svc's public API: each signs up, waits for the
//!    verification email that the worker's log sender delivers to the DEV ONLY mailbox,
//!    verifies with its link, signs in, registers a device, checks the access token
//!    against the JWKS and refreshes it;
//! 2. waits until the worker has relayed both users and devices from auth-svc's outbox to
//!    message-svc (message-svc's `Identify` accepts each device), then makes them accepted
//!    contacts with a direct conversation through message-svc's DEV ONLY `DevSetup` API
//!    (until contact requests, Plan 7);
//! 3. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify` naming
//!    the device auth-svc registered, and waits for `Ready`;
//! 4. sends a message from Alice and waits for her `SendAck`;
//! 5. waits for Bob's `Deliver` of that message;
//! 6. acks it from Bob, then pings: the `Pong` proves the ack went through, since the
//!    gateway handles a connection's frames in order and answers a failed ack with an
//!    `Error`.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`,
//! message-svc with `PUSH_DEV_SETUP=1` and the gateway with `PUSH_DEV_AUTH=1`.

mod auth;

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{IdentifyRequest, MakeContactsRequest, OpenDirectRequest};
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, Identify, Ping, Send};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tonic::transport::Endpoint;
use uuid::Uuid;

/// How long each step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep trying to connect: an ingress may answer 503 for a few seconds after
/// its backends become ready.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the worker may take to deliver an email or relay an event: it looks for work
/// every second.
pub(crate) const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a closing handshake may take; the test has passed by then.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where the deployment is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
    pub auth_url: String,
    /// Base URL of the worker, for the DEV ONLY mailbox of its log sender, which the
    /// ingress does not route: `http://push-worker:8080`.
    pub mailbox_url: String,
    /// gRPC URL of message-svc, to see the relayed devices and for the dev setup calls:
    /// `http://host:50051`.
    pub setup_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header for the WebSocket and public API requests, when the URLs name an
    /// ingress controller rather than the virtual host it routes (`push.local`).
    pub host: Option<String>,
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --setup <grpc-url> --socket <ws-url> [--host <host>]";

impl Target {
    /// Reads `--auth`, `--mailbox`, `--setup`, `--socket` and `--host` from `args`; each one
    /// missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`, `PUSH_SMOKE_MAILBOX_URL`,
    /// `PUSH_SMOKE_SETUP_URL`, `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_HOST`).
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (mut auth_url, mut mailbox_url) = (None, None);
        let (mut setup_url, mut socket_url, mut host) = (None, None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--auth" => &mut auth_url,
                "--mailbox" => &mut mailbox_url,
                "--setup" => &mut setup_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
                other => return Err(format!("unknown argument {other:?}; {USAGE}")),
            };
            *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        }
        let auth_url = auth_url
            .or_else(|| env("PUSH_SMOKE_AUTH_URL"))
            .ok_or_else(|| format!("--auth is required; {USAGE}"))?;
        let mailbox_url = mailbox_url
            .or_else(|| env("PUSH_SMOKE_MAILBOX_URL"))
            .ok_or_else(|| format!("--mailbox is required; {USAGE}"))?;
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
            auth_url,
            mailbox_url,
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
    let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
    let alice = auth.enroll("alice").await?;
    let bob = auth.enroll("bob").await?;

    let channel = Endpoint::from_shared(target.setup_url.clone())
        .with_context(|| format!("message-svc URL {}", target.setup_url))?
        .connect_timeout(STEP_TIMEOUT)
        .timeout(STEP_TIMEOUT)
        .connect()
        .await
        .with_context(|| format!("connect to message-svc at {}", target.setup_url))?;
    let alice = Client::relayed("alice", alice, &channel).await?;
    let bob = Client::relayed("bob", bob, &channel).await?;
    let mut setup = DevSetupClient::new(channel);
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
        "made alice={} and bob={} contacts with a direct conversation (DEV ONLY setup)",
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
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(None)).await;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

impl Client {
    /// Waits until the worker has relayed the user and device auth-svc enrolled to
    /// message-svc, which then accepts the device in `Identify`.
    async fn relayed(
        name: &'static str,
        enrolled: auth::Enrolled,
        channel: &tonic::transport::Channel,
    ) -> anyhow::Result<Self> {
        let (user, device) = (enrolled.user, enrolled.device);
        let mut messaging = MessagingClient::new(channel.clone());
        let started = Instant::now();
        let mut reported = None;
        loop {
            let identified = messaging
                .identify(IdentifyRequest {
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                })
                .await;
            match identified {
                Ok(_) => break,
                // Revoked, or another user's: waiting cannot help.
                Err(status) if status.code() == tonic::Code::PermissionDenied => bail!(
                    "message-svc refused {name}'s device {device} as revoked or another user's"
                ),
                // UNAVAILABLE is message-svc's answer for a device not relayed yet.
                Err(status) if started.elapsed() < WORKER_TIMEOUT => {
                    // Other codes (message-svc restarting, say) are reported once each.
                    if status.code() != tonic::Code::Unavailable && reported != Some(status.code())
                    {
                        reported = Some(status.code());
                        step(&format!(
                            "message-svc answers {:?} yet; retrying",
                            status.code()
                        ));
                    }
                }
                Err(status) => bail!(
                    "the worker did not relay {name}'s device to message-svc within {WORKER_TIMEOUT:?}: {:?}",
                    status.code()
                ),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        step(&format!(
            "the worker relayed {name}'s account and device {device} to message-svc (waited {} ms)",
            started.elapsed().as_millis()
        ));
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
                "--auth",
                "http://ingress",
                "--mailbox",
                "http://worker:8080",
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
                auth_url: "http://ingress".to_owned(),
                mailbox_url: "http://worker:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
            })
        );
    }

    #[test]
    fn missing_flags_come_from_the_environment() {
        let target = Target::parse(
            args(&["--socket", "ws://gw/v1/socket"]),
            |name| match name {
                "PUSH_SMOKE_AUTH_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://worker:8080".to_owned()),
                "PUSH_SMOKE_SETUP_URL" => Some("http://svc:50051".to_owned()),
                _ => None,
            },
        );
        assert_eq!(
            target,
            Ok(Target {
                auth_url: "http://auth:8080".to_owned(),
                mailbox_url: "http://worker:8080".to_owned(),
                setup_url: "http://svc:50051".to_owned(),
                socket_url: "ws://gw/v1/socket".to_owned(),
                host: None,
            })
        );
    }

    #[test]
    fn a_missing_url_or_unknown_flag_is_refused() {
        assert!(Target::parse(args(&["--setup", "http://svc:50051"]), |_| None).is_err());
        let without_auth = args(&[
            "--setup",
            "http://svc:50051",
            "--socket",
            "ws://gw/v1/socket",
        ]);
        assert!(Target::parse(without_auth, |_| None).is_err());
        assert!(Target::parse(args(&["--verbose"]), |_| None).is_err());
        assert!(Target::parse(args(&["--setup"]), |_| None).is_err());
    }
}
