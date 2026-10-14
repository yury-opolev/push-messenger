//! push-smoke: the end-to-end smoke test of a running push-messenger.
//!
//! Against a deployment it:
//!
//! 1. enrolls Alice, Bob and Carol through auth-svc's public API: each signs up, waits for
//!    the verification email that the worker's log sender delivers to the DEV ONLY
//!    mailbox, verifies with its link, signs in, registers a device, checks the access
//!    token against the JWKS and refreshes it;
//! 2. waits until the worker has relayed the users and devices from auth-svc's outbox to
//!    message-svc (message-svc's `Identify` accepts each device);
//! 3. over message-svc's `Messaging` API, called as the gateway calls it for an identified
//!    device (the gateway carries these frames from Plan 8): Alice asks Bob to be a
//!    contact and Bob accepts, Carol redeems Alice's invite, Alice opens the direct
//!    conversation with Bob; auth-svc shows Alice the profiles of Bob and Carol, and Bob
//!    none of Carol's;
//! 4. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify` naming
//!    the device auth-svc registered, and waits for `Ready`;
//! 5. sends a message from Alice and waits for her `SendAck` and Bob's `Deliver`; acks it
//!    from Bob, then pings: the `Pong` proves the ack went through, since the gateway
//!    handles a connection's frames in order and answers a failed ack with an `Error`;
//! 6. Alice edits the message and Bob's socket delivers the edit; Bob reads up to the
//!    message and Alice's listing shows it read; Alice creates a group with Bob and Carol,
//!    Bob's socket delivers its first entry and auth-svc now shows Bob Carol's profile;
//!    Alice deletes the message, Bob's socket delivers the delete, and Bob's history holds
//!    the message erased;
//! 7. Carol revokes her device and Bob resets his password through auth-svc; the worker
//!    relays both, message-svc kicks Carol's device (revoked) and Bob's (password reset) on
//!    their Redis channels, where the smoke test listens, and refuses Carol's device.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`, and
//! the gateway with `PUSH_DEV_AUTH=1`.

mod auth;
mod kicks;
mod messaging;

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{IdentifyRequest, KickReason};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Contact, ContactAccept, ContactRequest, ContactState, ContentType, CreateGroup,
    CreateInvite, Deliver, EntryKind, FetchHistory, Identify, InviteRedeemed, ListConversations,
    OpenDirect, Ping, RedeemInvite, Send, SendAck, conversation_summary, request, response,
};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::kicks::Kicks;
use crate::messaging::Messaging;
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
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
    pub auth_url: String,
    /// Base URL of the worker, for the DEV ONLY mailbox of its log sender, which the
    /// ingress does not route: `http://push-worker:8080`.
    pub mailbox_url: String,
    /// gRPC URL of message-svc, to see the relayed devices and to call its `Messaging` API
    /// as the gateway does: `http://host:50051`.
    pub message_svc_url: String,
    /// Redis URL, to hear the kicks message-svc publishes: `redis://:password@host:6379`.
    /// It holds a password, so it is never shown.
    pub redis_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header for the WebSocket and public API requests, when the URLs name an
    /// ingress controller rather than the virtual host it routes (`push.local`).
    pub host: Option<String>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Target")
            .field("auth_url", &self.auth_url)
            .field("mailbox_url", &self.mailbox_url)
            .field("message_svc_url", &self.message_svc_url)
            .field("redis_url", &"<redacted>")
            .field("socket_url", &self.socket_url)
            .field("host", &self.host)
            .finish()
    }
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --message-svc <grpc-url> --socket <ws-url> [--host <host>], with the Redis URL in PUSH_SMOKE_REDIS_URL (or --redis <url>)";

impl Target {
    /// Reads `--auth`, `--mailbox`, `--message-svc`, `--redis`, `--socket` and `--host`
    /// from `args`; each one missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`,
    /// `PUSH_SMOKE_MAILBOX_URL`, `PUSH_SMOKE_MESSAGE_SVC_URL`, `PUSH_SMOKE_REDIS_URL`,
    /// `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_HOST`). The Redis URL holds a password, so a
    /// deployment passes it in the environment, from its Secret.
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (mut auth_url, mut mailbox_url, mut message_svc_url) = (None, None, None);
        let (mut redis_url, mut socket_url, mut host) = (None, None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--auth" => &mut auth_url,
                "--mailbox" => &mut mailbox_url,
                "--message-svc" => &mut message_svc_url,
                "--redis" => &mut redis_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
                other => return Err(format!("unknown argument {other:?}; {USAGE}")),
            };
            *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        }
        let required = |value: Option<String>, flag: &str, variable: &str| {
            value
                .or_else(|| env(variable))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{flag} or {variable} is required; {USAGE}"))
        };
        Ok(Self {
            auth_url: required(auth_url, "--auth", "PUSH_SMOKE_AUTH_URL")?,
            mailbox_url: required(mailbox_url, "--mailbox", "PUSH_SMOKE_MAILBOX_URL")?,
            message_svc_url: required(
                message_svc_url,
                "--message-svc",
                "PUSH_SMOKE_MESSAGE_SVC_URL",
            )?,
            redis_url: required(redis_url, "--redis", "PUSH_SMOKE_REDIS_URL")?,
            socket_url: required(socket_url, "--socket", "PUSH_SMOKE_SOCKET_URL")?,
            host: host
                .or_else(|| env("PUSH_SMOKE_HOST"))
                .filter(|host| !host.is_empty()),
        })
    }
}
/// One user with one device.
struct Client {
    name: &'static str,
    user: Uuid,
    device: Uuid,
    email: String,
    /// Never printed.
    access_token: String,
}

/// Everyone in the smoke test.
struct People {
    alice: Client,
    bob: Client,
    carol: Client,
}

/// Runs the smoke test, printing each step. Fails at the first step that does not
/// complete within 10 s (30 s for what waits for the worker).
pub async fn run(target: &Target) -> anyhow::Result<()> {
    let started = Instant::now();
    let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
    let alice = auth.enroll("alice").await?;
    let bob = auth.enroll("bob").await?;
    let carol = auth.enroll("carol").await?;

    let channel = Endpoint::from_shared(target.message_svc_url.clone())
        .with_context(|| format!("message-svc URL {}", target.message_svc_url))?
        .connect_timeout(STEP_TIMEOUT)
        .timeout(STEP_TIMEOUT)
        .connect()
        .await
        .with_context(|| format!("connect to message-svc at {}", target.message_svc_url))?;
    let people = People {
        alice: Client::relayed("alice", alice, &channel).await?,
        bob: Client::relayed("bob", bob, &channel).await?,
        carol: Client::relayed("carol", carol, &channel).await?,
    };
    // Listening before anything can kick them.
    let mut kicks = tokio::time::timeout(
        STEP_TIMEOUT,
        Kicks::subscribe(&target.redis_url, &[people.bob.device, people.carol.device]),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Redis: no answer within {STEP_TIMEOUT:?}"))??;
    let mut api = Messaging::new(channel.clone());
    let conversation = connect(&mut api, &auth, &people).await?;

    let mut bob_socket = people.bob.connect(target).await?;
    let mut alice_socket = people.alice.connect(target).await?;
    let (sent, client_msg_id) = chat(
        &mut alice_socket,
        &mut bob_socket,
        &people.alice,
        &conversation,
    )
    .await?;
    let change = Change {
        conversation: &conversation,
        sent: &sent,
        client_msg_id: &client_msg_id,
    };
    change_messages(&mut api, &auth, &mut bob_socket, &people, &change).await?;
    for socket in [&mut alice_socket, &mut bob_socket] {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(None)).await;
    }

    kick(&mut kicks, &auth, &channel, &people).await?;
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

/// Contacts by request and by invite, the direct conversation, and the profiles each may
/// see. Returns the direct conversation of Alice and Bob.
async fn connect(
    api: &mut Messaging,
    auth: &auth::Auth<'_>,
    people: &People,
) -> anyhow::Result<Vec<u8>> {
    let People { alice, bob, carol } = people;
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob.user),
    });
    let asked = api
        .request(alice.user, ask, "alice's contact request")
        .await?;
    let pending = Contact {
        user_id: id_bytes(bob.user),
        state: ContactState::RequestedOut.into(),
    };
    if asked != response::Body::Contact(pending) {
        bail!("alice's contact request was answered with something else");
    }
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice.user),
    });
    api.request(bob.user, accept, "bob's accept").await?;
    step("alice asked bob to be a contact and bob accepted");

    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 600,
        max_uses: 1,
    });
    let response::Body::Invite(invite) = api.request(alice.user, create, "alice's invite").await?
    else {
        bail!("alice's invite was answered with something else");
    };
    let redeem = request::Body::RedeemInvite(RedeemInvite { code: invite.code });
    let redeemed = api.request(carol.user, redeem, "carol's redeem").await?;
    let creator = InviteRedeemed {
        user_id: id_bytes(alice.user),
    };
    if redeemed != response::Body::InviteRedeemed(creator) {
        bail!("carol's redeem was answered with something else");
    }
    step("carol redeemed alice's invite: they are contacts");

    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob.user),
    });
    let response::Body::DirectOpened(opened) =
        api.request(alice.user, open, "open the chat").await?
    else {
        bail!("opening the chat was answered with something else");
    };
    step("alice opened the direct conversation with bob");

    let mut seen = auth
        .profiles(&alice.access_token, &[bob.user, carol.user])
        .await?;
    seen.sort();
    let mut contacts = vec![bob.user, carol.user];
    contacts.sort();
    if seen != contacts {
        bail!("auth-svc showed alice the profiles of {seen:?}, not bob's and carol's");
    }
    // Asking for alice too proves the endpoint answers bob at all.
    if auth
        .profiles(&bob.access_token, &[alice.user, carol.user])
        .await?
        != [alice.user]
    {
        bail!("auth-svc did not show bob exactly alice's profile; carol shares nothing with him");
    }
    step(
        "auth-svc shows alice the profiles of her contacts bob and carol, and bob none of carol's",
    );
    Ok(opened.conversation_id)
}

/// Alice sends a message over the gateway and Bob receives and acks it: Alice's
/// `SendAck` and the message's `client_msg_id`.
async fn chat(
    alice_socket: &mut Socket,
    bob_socket: &mut Socket,
    alice: &Client,
    conversation: &[u8],
) -> anyhow::Result<(SendAck, Vec<u8>)> {
    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
    let sent_at = Instant::now();
    send(
        alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.to_vec(),
            content_type: ContentType::Text.into(),
            body: text.clone().into_bytes(),
            client_ts: None,
        }),
    )
    .await?;
    let ack = loop {
        match next(alice_socket, "alice's SendAck").await? {
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

    let entry = delivered(bob_socket, conversation, ack.seq, "bob's Deliver").await?;
    if entry.body.as_deref() != Some(text.as_bytes()) || entry.sender_id != id_bytes(alice.user) {
        bail!("bob received the wrong message: {}", describe(&entry));
    }
    step(&format!(
        "bob got Deliver seq={} after {} ms",
        entry.seq,
        sent_at.elapsed().as_millis()
    ));

    send(
        bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation.to_vec(),
            seq: entry.seq,
        }),
    )
    .await?;
    send(bob_socket, Body::Ping(Ping {})).await?;
    loop {
        match next(bob_socket, "bob's Pong").await? {
            Body::Pong(_) => break,
            Body::Error(error) => bail!("bob's ack failed: {error:?}"),
            _ => {}
        }
    }
    step("bob acked; the gateway answered the following Ping");
    Ok((ack, client_msg_id))
}

/// The message Alice sent in her direct conversation with Bob.
struct Change<'a> {
    conversation: &'a [u8],
    sent: &'a SendAck,
    client_msg_id: &'a [u8],
}

/// Alice edits the message, Bob reads it, Alice creates a group and deletes the message;
/// Bob's socket delivers every entry they add.
async fn change_messages(
    api: &mut Messaging,
    auth: &auth::Auth<'_>,
    bob_socket: &mut Socket,
    people: &People,
    message: &Change<'_>,
) -> anyhow::Result<()> {
    let People { alice, bob, carol } = people;
    let Change {
        conversation,
        sent,
        client_msg_id,
    } = *message;
    let edited = api
        .edit(
            alice.user,
            alice.device,
            conversation,
            &sent.message_id,
            "hello, edited",
        )
        .await?;
    if edited.seq != sent.seq + 1 {
        bail!("the edit got seq {}, not {}", edited.seq, sent.seq + 1);
    }
    let entry = delivered(bob_socket, conversation, edited.seq, "the edit").await?;
    if entry.kind() != EntryKind::Edit || entry.target_seq != Some(sent.seq) {
        bail!(
            "bob's socket delivered {} instead of the edit",
            describe(&entry)
        );
    }
    step(&format!(
        "alice edited her message; bob's socket delivered the edit (seq {})",
        edited.seq
    ));

    if !api.read_up_to(bob.user, conversation, sent.seq).await? {
        bail!("bob's read position did not move");
    }
    let list = request::Body::ListConversations(ListConversations::default());
    let response::Body::Conversations(listed) =
        api.request(alice.user, list, "alice's listing").await?
    else {
        bail!("alice's listing was answered with something else");
    };
    let read = listed
        .conversations
        .iter()
        .find(|summary| summary.conversation_id == conversation)
        .and_then(|summary| match &summary.receipts {
            Some(conversation_summary::Receipts::Direct(receipts)) => Some(receipts.peer_read_seq),
            _ => None,
        });
    if read != Some(sent.seq) {
        bail!(
            "alice's listing shows bob read up to {read:?}, not {}",
            sent.seq
        );
    }
    step("bob read the message; alice's listing shows it read");

    let create = request::Body::CreateGroup(CreateGroup {
        title: "Smoke".to_owned(),
        member_ids: vec![id_bytes(bob.user), id_bytes(carol.user)],
    });
    let response::Body::GroupCreated(group) =
        api.request(alice.user, create, "create a group").await?
    else {
        bail!("creating a group was answered with something else");
    };
    let entry = delivered(bob_socket, &group.conversation_id, 1, "the group").await?;
    if entry.kind() != EntryKind::System {
        bail!(
            "the group's first entry is {}, not a system entry",
            describe(&entry)
        );
    }
    if auth.profiles(&bob.access_token, &[carol.user]).await? != [carol.user] {
        bail!("auth-svc does not show bob carol's profile, though they share a group");
    }
    step(
        "alice created a group with bob and carol; bob's socket delivered its first entry and auth-svc now shows bob carol's profile",
    );

    let deleted = api
        .delete(alice.user, alice.device, conversation, client_msg_id)
        .await?;
    if deleted.seq != edited.seq + 1 {
        bail!("the delete got seq {}, not {}", deleted.seq, edited.seq + 1);
    }
    let entry = delivered(bob_socket, conversation, deleted.seq, "the delete").await?;
    if entry.kind() != EntryKind::Delete || entry.target_seq != Some(sent.seq) {
        bail!(
            "bob's socket delivered {} instead of the delete",
            describe(&entry)
        );
    }
    let fetch = request::Body::FetchHistory(FetchHistory {
        conversation_id: conversation.to_vec(),
        before_seq: None,
        limit: 0,
    });
    let response::Body::History(page) = api.request(bob.user, fetch, "bob's history").await? else {
        bail!("bob's history was answered with something else");
    };
    match page.entries.as_slice() {
        [message] if message.seq == sent.seq && message.deleted && message.body.is_none() => {}
        entries => bail!(
            "bob's history is not the erased message alone ({} entries)",
            entries.len()
        ),
    }
    step(&format!(
        "alice deleted her message; bob's socket delivered the delete (seq {}) and his history holds it erased",
        deleted.seq
    ));
    Ok(())
}

/// Carol revokes her device and Bob resets his password; message-svc kicks both devices
/// once the worker relays the events, and refuses Carol's device from then on.
async fn kick(
    kicks: &mut Kicks,
    auth: &auth::Auth<'_>,
    channel: &Channel,
    people: &People,
) -> anyhow::Result<()> {
    let People { bob, carol, .. } = people;
    auth.revoke_device(&carol.access_token, carol.device)
        .await?;
    let waited = kicks
        .kicked(carol.device, KickReason::DeviceRevoked)
        .await?;
    let identified = MessagingClient::new(channel.clone())
        .identify(IdentifyRequest {
            user_id: id_bytes(carol.user),
            device_id: id_bytes(carol.device),
        })
        .await;
    match identified {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {}
        Err(status) => bail!(
            "message-svc answered {:?} for carol's revoked device",
            status.code()
        ),
        Ok(_) => bail!("message-svc still accepts carol's revoked device"),
    }
    step(&format!(
        "carol revoked her device; the worker relayed it and message-svc kicked it on Redis after {} ms, and refuses it now",
        waited.as_millis()
    ));

    auth.reset_password(&bob.email).await?;
    let waited = kicks.kicked(bob.device, KickReason::PasswordReset).await?;
    step(&format!(
        "bob reset his password; the worker relayed it and message-svc kicked his device on Redis after {} ms",
        waited.as_millis()
    ));
    Ok(())
}

/// Waits for the `Deliver` of `seq` in `conversation` on `socket`.
async fn delivered(
    socket: &mut Socket,
    conversation: &[u8],
    seq: i64,
    waiting_for: &str,
) -> anyhow::Result<Deliver> {
    loop {
        match next(socket, waiting_for).await? {
            Body::Deliver(entry) if entry.seq == seq && entry.conversation_id == conversation => {
                return Ok(entry);
            }
            Body::Error(error) => bail!("an error while waiting for {waiting_for}: {error:?}"),
            _ => {}
        }
    }
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
        Ok(Self {
            name,
            user,
            device,
            email: enrolled.email,
            access_token: enrolled.access_token,
        })
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
            _ => bail!("{} expected Ready, got another frame", self.name),
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

/// An entry without its body, which may be a message.
fn describe(entry: &Deliver) -> String {
    format!(
        "an entry of kind {:?} (seq {}, target_seq {:?})",
        entry.kind(),
        entry.seq,
        entry.target_seq
    )
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
                "--message-svc",
                "http://svc:50051",
                "--redis",
                "redis://:secret@redis:6379",
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
                message_svc_url: "http://svc:50051".to_owned(),
                redis_url: "redis://:secret@redis:6379".to_owned(),
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
                "PUSH_SMOKE_MESSAGE_SVC_URL" => Some("http://svc:50051".to_owned()),
                "PUSH_SMOKE_REDIS_URL" => Some("redis://:secret@redis:6379".to_owned()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(target.message_svc_url, "http://svc:50051");
        assert_eq!(target.redis_url, "redis://:secret@redis:6379");
        assert_eq!(target.host, None);
        assert!(!format!("{target:?}").contains("secret"));
    }

    #[test]
    fn a_missing_url_or_unknown_flag_is_refused() {
        let all = |name: &str| Some(format!("value of {name}"));
        assert!(Target::parse(args(&[]), all).is_ok());
        for missing in [
            "PUSH_SMOKE_AUTH_URL",
            "PUSH_SMOKE_MAILBOX_URL",
            "PUSH_SMOKE_MESSAGE_SVC_URL",
            "PUSH_SMOKE_REDIS_URL",
            "PUSH_SMOKE_SOCKET_URL",
        ] {
            let error =
                Target::parse(args(&[]), |name| all(name).filter(|_| name != missing)).unwrap_err();
            assert!(error.contains(missing), "{error}");
        }
        assert!(Target::parse(args(&["--verbose"]), all).is_err());
        assert!(Target::parse(args(&["--message-svc"]), all).is_err());
    }
}
