//! push-smoke: the end-to-end smoke test of a running push-messenger.
//!
//! Against a deployment it:
//!
//! 1. enrolls Alice, Bob and Carol through auth-svc's public API: each signs up, waits for
//!    the verification email that the worker's log sender delivers to the DEV ONLY
//!    mailbox, verifies with its link, signs in, registers a device, checks the access
//!    token against the JWKS and refreshes it;
//! 2. connects each device's WebSocket with its access token, as an app does, and waits
//!    for `Ready`: until the worker has relayed the device to message-svc the gateway
//!    closes with 4006 ("not ready yet"), and the smoke test reconnects with backoff;
//! 3. over the WebSocket: Alice asks Bob to be a contact, Bob's socket hears the request
//!    and Bob accepts; Carol redeems Alice's invite; Alice opens the direct conversation;
//!    auth-svc shows Alice the profiles of Bob and Carol, and Bob none of Carol's;
//! 4. Alice sends a message and gets her `SendAck`; Bob's socket delivers it and Bob acks;
//! 5. Alice edits the message and Bob's socket delivers the edit; Bob reads up to the
//!    message, his socket hears his new read position and Alice's the receipt, and her
//!    listing shows it read; a group request is refused (groups are not offered to apps
//!    in the first release); Alice deletes the message, Bob's socket delivers the delete,
//!    and Bob's history holds the message erased;
//! 6. tokens signed with auth-svc's key but expired, or for another audience, are refused
//!    (4002, 4001);
//! 7. Carol revokes her device and Bob resets his password through auth-svc; once the
//!    worker relays them, message-svc kicks both devices and the gateway closes Carol's
//!    socket with 4003 and Bob's with 4001, and refuses their old tokens from then on;
//! 8. Dana and Erin do it all again as apps built on `push-sdk` (`sdk.rs`): signing up and
//!    in, an invite, a chat with receipts, an edit and a delete, an outbox that survives
//!    restarts, and the SDK's `DeviceRemoved` and `SignedOut`.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`. The
//! smoke test needs auth-svc's signing key (from the deployment's Secret) for step 6 and
//! a writable directory for the SDK's databases.
//!
//! A Microsoft Entra ID-only deployment is tested by [`entra::run`] instead (DEV ONLY,
//! with the rehearsal issuer's token endpoint).

mod auth;
pub mod entra;
mod sdk;
mod socket;
mod tokens;

use std::time::{Duration, Instant};

use anyhow::bail;
use push_proto::id_bytes;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Contact, ContactAccept, ContactEvent, ContactRequest, ContactState, ContentType,
    CreateGroup, CreateInvite, Delete, Deliver, Edit, EntryKind, ErrorCode, FetchHistory,
    InviteRedeemed, ListConversations, OpenDirect, ReadUpTo, RedeemInvite, Send, SendAck,
    conversation_summary, delete, edit, receipt, request, response,
};
use uuid::Uuid;

use crate::socket::{Conn, Identified, identify};
use crate::tokens::Forger;

/// How long each step may take.
pub(crate) const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep trying to connect: an ingress may answer 503 for a few seconds after
/// its backends become ready.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the worker may take to deliver an email or relay an event: it looks for work
/// every second.
pub(crate) const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the deployment is.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
    pub auth_url: String,
    /// Base URL of the worker, for the DEV ONLY mailbox of its log sender, which the
    /// ingress does not route: `http://push-worker:8080`.
    pub mailbox_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header for the WebSocket and public API requests, when the URLs name an
    /// ingress controller rather than the virtual host it routes (`push.local`).
    pub host: Option<String>,
    /// auth-svc's signing key (base64 of the 32-byte Ed25519 seed), to sign tokens the
    /// gateway must refuse. It can sign in as anyone, so it is never shown.
    pub signing_key: String,
    /// A writable directory for the SDK apps' databases.
    pub data_dir: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Target")
            .field("auth_url", &self.auth_url)
            .field("mailbox_url", &self.mailbox_url)
            .field("socket_url", &self.socket_url)
            .field("host", &self.host)
            .field("signing_key", &"<redacted>")
            .field("data_dir", &self.data_dir)
            .finish()
    }
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --socket <ws-url> --data-dir <dir> [--host <host>], with auth-svc's signing key in PUSH_SMOKE_SIGNING_KEY";

impl Target {
    /// Reads `--auth`, `--mailbox`, `--socket`, `--data-dir` and `--host` from `args`;
    /// each one missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`,
    /// `PUSH_SMOKE_MAILBOX_URL`, `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_DATA_DIR`,
    /// `PUSH_SMOKE_HOST`). The signing key comes from
    /// `PUSH_SMOKE_SIGNING_KEY` only: a secret never belongs in a Pod's arguments.
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (mut auth_url, mut mailbox_url, mut socket_url, mut host) = (None, None, None, None);
        let mut data_dir = None;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--auth" => &mut auth_url,
                "--mailbox" => &mut mailbox_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
                "--data-dir" => &mut data_dir,
                other => return Err(format!("unknown argument {other:?}; {USAGE}")),
            };
            *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        }
        let required = |value: Option<String>, what: &str, variable: &str| {
            value
                .or_else(|| env(variable))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{what} is required; {USAGE}"))
        };
        Ok(Self {
            auth_url: required(
                auth_url,
                "--auth or PUSH_SMOKE_AUTH_URL",
                "PUSH_SMOKE_AUTH_URL",
            )?,
            mailbox_url: required(
                mailbox_url,
                "--mailbox or PUSH_SMOKE_MAILBOX_URL",
                "PUSH_SMOKE_MAILBOX_URL",
            )?,
            socket_url: required(
                socket_url,
                "--socket or PUSH_SMOKE_SOCKET_URL",
                "PUSH_SMOKE_SOCKET_URL",
            )?,
            host: host
                .or_else(|| env("PUSH_SMOKE_HOST"))
                .filter(|host| !host.is_empty()),
            signing_key: required(None, "PUSH_SMOKE_SIGNING_KEY", "PUSH_SMOKE_SIGNING_KEY")?,
            data_dir: required(
                data_dir,
                "--data-dir or PUSH_SMOKE_DATA_DIR",
                "PUSH_SMOKE_DATA_DIR",
            )?,
        })
    }
}

/// One user with one device and its open connection.
struct Client {
    name: &'static str,
    user: Uuid,
    device: Uuid,
    email: String,
    /// Never printed.
    access_token: String,
    conn: Box<Conn>,
}

/// Runs the smoke test, printing each step: the raw protocol, then the SDK. Fails at the
/// first step that does not complete within 10 s (30 s for what waits for the worker).
pub async fn run(target: &Target) -> anyhow::Result<()> {
    let started = Instant::now();
    let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
    let alice = auth.enroll("alice").await?;
    let bob = auth.enroll("bob").await?;
    let carol = auth.enroll("carol").await?;
    let forger = Forger::new(&target.signing_key, &alice.access_token)?;

    let mut alice = Client::connect(target, "alice", alice).await?;
    let mut bob = Client::connect(target, "bob", bob).await?;
    let mut carol = Client::connect(target, "carol", carol).await?;
    let conversation = befriend(&auth, &mut alice, &mut bob, &mut carol).await?;
    let (sent, client_msg_id) = chat(&mut alice, &mut bob, &conversation).await?;
    let message = Message {
        conversation: &conversation,
        sent: &sent,
        client_msg_id: &client_msg_id,
    };
    change(&mut alice, &mut bob, &carol, &message).await?;
    refuse_tokens(target, &auth, &forger, &alice).await?;
    kick(target, &auth, bob, carol).await?;
    alice.conn.close().await;
    sdk::run(target, &auth).await?;
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

impl Client {
    /// Connects and identifies with the device's access token. Until the worker has
    /// relayed the device to message-svc the gateway closes with 4006, which means "try
    /// again shortly", never a sign-out: reconnects with backoff, 250 ms doubling to 2 s.
    async fn connect(
        target: &Target,
        name: &'static str,
        enrolled: auth::Enrolled,
    ) -> anyhow::Result<Self> {
        let started = Instant::now();
        let mut not_ready = 0;
        let mut backoff = Duration::from_millis(250);
        let (conn, ready) = loop {
            match identify(target, name, &enrolled.access_token).await? {
                Identified::Ready(conn, ready) => break (conn, ready),
                Identified::Closed(4006) if started.elapsed() < WORKER_TIMEOUT => {
                    not_ready += 1;
                }
                Identified::Closed(4006) => bail!(
                    "the worker did not relay {name}'s device to message-svc within {WORKER_TIMEOUT:?}"
                ),
                Identified::Closed(code) => bail!("{name}'s Identify was closed with {code}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        };
        if ready.user_id != id_bytes(enrolled.user) || ready.device_id != id_bytes(enrolled.device)
        {
            bail!("{name}'s Ready names another user or device than the token");
        }
        step(&format!(
            "{name} connected with the access token (Ready, heartbeat {} ms) once the worker had relayed device {}: {not_ready} \"not ready yet\" closes (4006) in {} ms",
            ready.heartbeat_ms,
            enrolled.device,
            started.elapsed().as_millis()
        ));
        Ok(Self {
            name,
            user: enrolled.user,
            device: enrolled.device,
            email: enrolled.email,
            access_token: enrolled.access_token,
            conn,
        })
    }
}

/// Contacts by request and by invite, the direct conversation, and the profiles each may
/// see, all over the WebSocket. Returns the direct conversation of Alice and Bob.
async fn befriend(
    auth: &auth::Auth<'_>,
    alice: &mut Client,
    bob: &mut Client,
    carol: &mut Client,
) -> anyhow::Result<Vec<u8>> {
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob.user),
    });
    let asked = alice.conn.request(ask, "alice's contact request").await?;
    let pending = Contact {
        user_id: id_bytes(bob.user),
        state: ContactState::RequestedOut.into(),
    };
    if asked != response::Body::Contact(pending) {
        bail!("alice's contact request was answered with something else");
    }
    let heard = contact_event(alice.user, ContactState::RequestedIn);
    bob.conn
        .wait_for("the request's ContactEvent", |frame| *frame == heard)
        .await?;
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice.user),
    });
    bob.conn.request(accept, "bob's accept").await?;
    let accepted = contact_event(bob.user, ContactState::Accepted);
    alice
        .conn
        .wait_for("the acceptance's ContactEvent", |frame| *frame == accepted)
        .await?;
    step(
        "alice asked bob to be a contact; bob's socket heard it (ContactEvent), bob accepted and alice's socket heard that",
    );

    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 600,
        max_uses: 1,
    });
    let response::Body::Invite(invite) = alice.conn.request(create, "alice's invite").await? else {
        bail!("alice's invite was answered with something else");
    };
    let redeem = request::Body::RedeemInvite(RedeemInvite { code: invite.code });
    let redeemed = carol.conn.request(redeem, "carol's redeem").await?;
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
    let response::Body::DirectOpened(opened) = alice.conn.request(open, "open the chat").await?
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

/// DEV ONLY: `count` new verified accounts for push-load, through auth-svc's public API
/// and the worker's dev mailbox. auth-svc allows 20 signups per IP an hour.
pub async fn create_verified_accounts(
    target: &Target,
    count: usize,
) -> anyhow::Result<Vec<push_load::accounts::Account>> {
    let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
    let mut accounts = Vec::with_capacity(count);
    for index in 0..count {
        let (email, password) = auth.sign_up_verified(&format!("load{index}")).await?;
        accounts.push(push_load::accounts::Account { email, password });
    }
    Ok(accounts)
}

fn contact_event(user: Uuid, state: ContactState) -> Body {
    Body::ContactEvent(ContactEvent {
        user_id: id_bytes(user),
        state: state.into(),
    })
}

/// Alice sends a message and Bob receives and acks it: Alice's `SendAck` and the
/// message's `client_msg_id`.
async fn chat(
    alice: &mut Client,
    bob: &mut Client,
    conversation: &[u8],
) -> anyhow::Result<(SendAck, Vec<u8>)> {
    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
    let sent_at = Instant::now();
    alice
        .conn
        .send(Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.to_vec(),
            content_type: ContentType::Text.into(),
            body: text.clone().into_bytes(),
            client_ts: None,
        }))
        .await?;
    let Body::SendAck(ack) = alice
        .conn
        .wait_for(
            "alice's SendAck",
            |frame| matches!(frame, Body::SendAck(ack) if ack.client_msg_id == client_msg_id),
        )
        .await?
    else {
        unreachable!("the wait accepts only SendAck");
    };
    step(&format!(
        "alice got SendAck seq={} after {} ms",
        ack.seq,
        sent_at.elapsed().as_millis()
    ));

    let entry = delivered(&mut bob.conn, conversation, ack.seq, "bob's Deliver").await?;
    if entry.body.as_deref() != Some(text.as_bytes()) || entry.sender_id != id_bytes(alice.user) {
        bail!("bob received the wrong message: {}", describe(&entry));
    }
    step(&format!(
        "bob got Deliver seq={} after {} ms",
        entry.seq,
        sent_at.elapsed().as_millis()
    ));

    bob.conn
        .send(Body::Ack(Ack {
            conversation_id: conversation.to_vec(),
            seq: entry.seq,
        }))
        .await?;
    bob.conn.ping().await?;
    step("bob acked; the gateway answered the following Ping");
    Ok((ack, client_msg_id))
}

/// The message Alice sent in her direct conversation with Bob.
struct Message<'a> {
    conversation: &'a [u8],
    sent: &'a SendAck,
    client_msg_id: &'a [u8],
}

/// Alice edits the message, Bob reads it, a group request is refused and Alice deletes
/// the message; Bob's socket delivers every entry they add.
async fn change(
    alice: &mut Client,
    bob: &mut Client,
    carol: &Client,
    message: &Message<'_>,
) -> anyhow::Result<()> {
    let Message {
        conversation,
        sent,
        client_msg_id,
    } = *message;
    let edit_id = id_bytes(Uuid::now_v7());
    alice
        .conn
        .send(Body::Edit(Edit {
            client_msg_id: edit_id.clone(),
            conversation_id: conversation.to_vec(),
            target: Some(edit::Target::TargetMessageId(sent.message_id.clone())),
            body: b"hello, edited".to_vec(),
            client_ts: None,
        }))
        .await?;
    let edited = acked(&mut alice.conn, &edit_id, "the edit's SendAck").await?;
    if edited.seq != sent.seq + 1 {
        bail!("the edit got seq {}, not {}", edited.seq, sent.seq + 1);
    }
    let entry = delivered(&mut bob.conn, conversation, edited.seq, "the edit").await?;
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

    bob.conn
        .send(Body::ReadUpTo(ReadUpTo {
            conversation_id: conversation.to_vec(),
            seq: sent.seq,
        }))
        .await?;
    bob.conn
        .wait_for("bob's ReadPosition", |frame| {
            matches!(frame, Body::ReadPosition(read)
                if read.conversation_id == conversation && read.read_seq == sent.seq)
        })
        .await?;
    alice
        .conn
        .wait_for("the read receipt", |frame| {
            matches!(frame, Body::Receipt(receipt)
                if receipt.conversation_id == conversation
                    && matches!(&receipt.positions,
                        Some(receipt::Positions::Direct(direct)) if direct.read_seq == sent.seq))
        })
        .await?;
    let list = request::Body::ListConversations(ListConversations::default());
    let response::Body::Conversations(listed) = alice.conn.request(list, "alice's listing").await?
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
    step(
        "bob read the message; his socket heard his ReadPosition, alice's the Receipt, and her listing shows it read",
    );

    let group = request::Body::CreateGroup(CreateGroup {
        title: "Smoke".to_owned(),
        member_ids: vec![id_bytes(bob.user), id_bytes(carol.user)],
    });
    match alice.conn.ask(group, "a group request").await? {
        Err(error) if error.code() == ErrorCode::NotAvailable => {}
        Err(error) => bail!("a group request was refused with {:?}", error.code()),
        Ok(_) => bail!("a group was created, though groups are not offered to apps"),
    }
    step("alice's group request was refused (NOT_AVAILABLE): groups are not offered to apps");

    let delete_id = id_bytes(Uuid::now_v7());
    alice
        .conn
        .send(Body::Delete(Delete {
            client_msg_id: delete_id.clone(),
            conversation_id: conversation.to_vec(),
            target: Some(delete::Target::TargetClientMsgId(client_msg_id.to_vec())),
        }))
        .await?;
    let deleted = acked(&mut alice.conn, &delete_id, "the delete's SendAck").await?;
    if deleted.seq != edited.seq + 1 {
        bail!("the delete got seq {}, not {}", deleted.seq, edited.seq + 1);
    }
    let entry = delivered(&mut bob.conn, conversation, deleted.seq, "the delete").await?;
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
    let response::Body::History(page) = bob.conn.request(fetch, "bob's history").await? else {
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

/// Tokens signed with auth-svc's key and naming Alice's device, but expired or for
/// another audience, are refused: 4002 (refresh, then reconnect) and 4001 (sign in again).
async fn refuse_tokens(
    target: &Target,
    auth: &auth::Auth<'_>,
    forger: &Forger,
    alice: &Client,
) -> anyhow::Result<()> {
    let audience = auth.expected_audience();
    // Expired two minutes ago: past the gateway's 60 s leeway.
    let expired = forger.token(alice.user, alice.device, 120, &audience);
    let elsewhere = "wss://elsewhere.invalid/v1/socket";
    let other_audience = forger.token(alice.user, alice.device, -600, elsewhere);
    for (token, expected, what) in [
        (expired, 4002, "an expired token"),
        (other_audience, 4001, "a token for another audience"),
    ] {
        match identify(target, "alice", &token).await? {
            Identified::Closed(code) if code == expected => {}
            Identified::Closed(code) => bail!("{what} was closed with {code}, not {expected}"),
            Identified::Ready(..) => bail!("the gateway accepted {what}"),
        }
    }
    step(
        "the gateway refused a token that had expired (4002) and one for another audience (4001), both signed with auth-svc's key",
    );
    Ok(())
}

/// Carol revokes her device and Bob resets his password; once the worker relays them
/// the gateway closes their sockets (4003 for the revocation, 4001 for the reset) and
/// refuses their old tokens.
async fn kick(
    target: &Target,
    auth: &auth::Auth<'_>,
    mut bob: Client,
    mut carol: Client,
) -> anyhow::Result<()> {
    // Both sockets have been quiet; a Ping keeps them clear of the idle timeout.
    carol.conn.ping().await?;
    bob.conn.ping().await?;

    let started = Instant::now();
    auth.revoke_device(&carol.access_token, carol.device)
        .await?;
    let code = carol.conn.closed("carol's kick", WORKER_TIMEOUT).await?;
    if code != 4003 {
        bail!("carol's socket was closed with {code}, not 4003 (revoked)");
    }
    let waited = started.elapsed();
    match identify(target, carol.name, &carol.access_token).await? {
        Identified::Closed(4003) => {}
        Identified::Closed(code) => bail!("carol's revoked device was closed with {code}"),
        Identified::Ready(..) => bail!("the gateway still accepts carol's revoked device"),
    }
    step(&format!(
        "carol revoked her device; after the worker relayed it the gateway closed her socket with 4003 ({} ms) and refuses her token",
        waited.as_millis()
    ));

    let started = Instant::now();
    auth.reset_password(&bob.email).await?;
    let code = bob.conn.closed("bob's kick", WORKER_TIMEOUT).await?;
    if code != 4001 {
        bail!("bob's socket was closed with {code}, not 4001 (sign in again)");
    }
    let waited = started.elapsed();
    match identify(target, bob.name, &bob.access_token).await? {
        Identified::Closed(4001) => {}
        Identified::Closed(code) => {
            bail!("bob's token from before the reset was closed with {code}")
        }
        Identified::Ready(..) => {
            bail!("the gateway still accepts bob's token from before the reset")
        }
    }
    step(&format!(
        "bob reset his password; after the worker relayed it the gateway closed his socket with 4001 ({} ms) and refuses his token from before the reset",
        waited.as_millis()
    ));
    Ok(())
}

/// Waits for the `SendAck` of the edit or delete sent with `client_msg_id`.
async fn acked(conn: &mut Conn, client_msg_id: &[u8], what: &str) -> anyhow::Result<SendAck> {
    let frame = conn
        .wait_for(
            what,
            |frame| matches!(frame, Body::SendAck(ack) if ack.client_msg_id == client_msg_id),
        )
        .await?;
    let Body::SendAck(ack) = frame else {
        unreachable!("the wait accepts only SendAck");
    };
    Ok(ack)
}

/// Waits for the `Deliver` of `seq` in `conversation`.
async fn delivered(
    conn: &mut Conn,
    conversation: &[u8],
    seq: i64,
    waiting_for: &str,
) -> anyhow::Result<Deliver> {
    let frame = conn
        .wait_for(waiting_for, |frame| {
            matches!(frame, Body::Deliver(entry)
                if entry.seq == seq && entry.conversation_id == conversation)
        })
        .await?;
    let Body::Deliver(entry) = frame else {
        unreachable!("the wait accepts only Deliver");
    };
    Ok(entry)
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

    fn key(name: &str) -> Option<String> {
        (name == "PUSH_SMOKE_SIGNING_KEY").then(|| "c2VjcmV0LWtleQ==".to_owned())
    }

    #[test]
    fn flags_name_the_target() {
        let target = Target::parse(
            args(&[
                "--auth",
                "http://ingress",
                "--mailbox",
                "http://worker:8080",
                "--socket",
                "ws://ingress/v1/socket",
                "--host",
                "push.local",
                "--data-dir",
                "/data",
            ]),
            key,
        );
        assert_eq!(
            target,
            Ok(Target {
                auth_url: "http://ingress".to_owned(),
                mailbox_url: "http://worker:8080".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
                signing_key: "c2VjcmV0LWtleQ==".to_owned(),
                data_dir: "/data".to_owned(),
            })
        );
    }

    #[test]
    fn missing_flags_come_from_the_environment_and_the_key_never_shows() {
        let target = Target::parse(
            args(&["--socket", "ws://gw/v1/socket"]),
            |name| match name {
                "PUSH_SMOKE_AUTH_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://worker:8080".to_owned()),
                "PUSH_SMOKE_DATA_DIR" => Some("/data".to_owned()),
                other => key(other),
            },
        )
        .unwrap();
        assert_eq!(target.auth_url, "http://auth:8080");
        assert_eq!(target.host, None);
        assert!(!format!("{target:?}").contains("c2VjcmV0"));
    }

    #[test]
    fn a_missing_url_or_key_or_an_unknown_flag_is_refused() {
        let all = |name: &str| Some(format!("value of {name}"));
        assert!(Target::parse(args(&[]), all).is_ok());
        for missing in [
            "PUSH_SMOKE_AUTH_URL",
            "PUSH_SMOKE_MAILBOX_URL",
            "PUSH_SMOKE_SOCKET_URL",
            "PUSH_SMOKE_SIGNING_KEY",
            "PUSH_SMOKE_DATA_DIR",
        ] {
            let error =
                Target::parse(args(&[]), |name| all(name).filter(|_| name != missing)).unwrap_err();
            assert!(error.contains(missing), "{error}");
        }
        assert!(Target::parse(args(&["--verbose"]), all).is_err());
        assert!(Target::parse(args(&["--signing-key", "x"]), all).is_err());
        assert!(Target::parse(args(&["--message-svc", "http://svc:50051"]), all).is_err());
    }
}
