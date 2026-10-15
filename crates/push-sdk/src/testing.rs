//! Test doubles: a gateway driven frame by frame and auth-svc answering with counters, for
//! the SDK's own tests; and [`AutoGateway`], a gateway that answers by itself, for the
//! bindings' tests (feature `testing`, never in an app).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, post};
use futures_util::{SinkExt, StreamExt};
use push_domain::{ConversationId, DeviceId, UserId};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    ContactList, ContentType, ConversationList, Deliver, DirectOpened, EntryKind, ErrorCode,
    InviteRedeemed, Pong, Ready, Request, Response, SendAck, request, response,
};
use push_proto::{decode_frame, encode_frame};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_async};
use uuid::Uuid;

pub type Server = WebSocketStream<TcpStream>;

/// A gateway the test drives frame by frame: each accepted connection is handed over.
pub struct FakeGateway {
    pub url: String,
    connections: mpsc::UnboundedReceiver<Server>,
}

impl FakeGateway {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
        let (sender, connections) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if let Ok(socket) = accept_async(stream).await {
                    let _ = sender.send(socket);
                }
            }
        });
        Self { url, connections }
    }

    pub async fn next(&mut self) -> Server {
        timeout(Duration::from_secs(10), self.connections.recv())
            .await
            .expect("a connection within 10 s")
            .unwrap()
    }

    /// No connection comes within `wait`.
    pub async fn quiet(&mut self, wait: Duration) {
        assert!(timeout(wait, self.connections.recv()).await.is_err());
    }
}

/// The next frame from the client.
pub async fn recv(socket: &mut Server) -> Body {
    loop {
        let message = timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a frame within 10 s")
            .unwrap()
            .unwrap();
        if let Message::Binary(bytes) = message {
            return decode_frame(&bytes).unwrap();
        }
    }
}

/// The next frame `wanted` accepts, skipping the others.
pub async fn recv_until(socket: &mut Server, wanted: impl Fn(&Body) -> bool) -> Body {
    loop {
        let body = recv(socket).await;
        if wanted(&body) {
            return body;
        }
    }
}

pub async fn send(socket: &mut Server, body: Body) {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .unwrap();
}

/// Like [`send`], but false instead of a panic once the connection is gone.
pub async fn try_send(socket: &mut Server, body: Body) -> bool {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .is_ok()
}

pub async fn close(socket: &mut Server, code: u16) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    };
    let _ = socket.close(Some(frame)).await;
}

/// auth-svc for one user and device: sign-in, registration, refresh (answering
/// `access-N`) and revocation, each counted.
pub struct FakeAuth {
    pub url: String,
    pub user: UserId,
    pub device: DeviceId,
    /// Who the tokens it issues name from now on; starts as `user` and `device`.
    pub identity: Arc<Mutex<(UserId, DeviceId)>>,
    pub registrations: Arc<AtomicUsize>,
    pub refreshes: Arc<AtomicUsize>,
    pub device_logins: Arc<AtomicUsize>,
    pub revocations: Arc<AtomicUsize>,
    /// How long a refresh takes after auth-svc counted (and rotated) it.
    pub refresh_delay_ms: Arc<AtomicU64>,
    /// Revocations fail with 500 while set.
    pub fail_revocations: Arc<AtomicBool>,
    /// The next registration answers `device_not_found`, as for a revoked device's id.
    pub spend_next_registration: Arc<AtomicBool>,
    /// The `registration_id` of every registration, in order.
    pub registration_ids: Arc<Mutex<Vec<String>>>,
    /// Sign-ups answered.
    pub signups: Arc<AtomicUsize>,
}

/// The password [`FakeAuth`] refuses with `invalid_credentials`; it accepts any other.
pub const WRONG_PASSWORD: &str = "wrong password";

impl FakeAuth {
    pub async fn start() -> Self {
        let (user, device) = (UserId::new(), DeviceId::new());
        let registrations = Arc::new(AtomicUsize::new(0));
        let refreshes = Arc::new(AtomicUsize::new(0));
        let device_logins = Arc::new(AtomicUsize::new(0));
        let revocations = Arc::new(AtomicUsize::new(0));
        let refresh_delay_ms = Arc::new(AtomicU64::new(0));
        let fail_revocations = Arc::new(AtomicBool::new(false));
        let spend_next_registration = Arc::new(AtomicBool::new(false));
        let registration_ids = Arc::new(Mutex::new(Vec::new()));
        let signups = Arc::new(AtomicUsize::new(0));
        let signed_up = signups.clone();
        let identity = Arc::new(Mutex::new((user, device)));
        let current = identity.clone();
        let tokens = move |n: usize| {
            let (user, device) = *current.lock().unwrap();
            json!({
                "user_id": user,
                "device_id": device,
                "access_token": format!("access-{n}"),
                "expires_in_s": 900,
                "refresh_token": format!("refresh-{n}"),
            })
        };
        let (logins, registered, refreshed, revoked, delay) = (
            device_logins.clone(),
            registrations.clone(),
            refreshes.clone(),
            revocations.clone(),
            refresh_delay_ms.clone(),
        );
        let (fail, spend, ids) = (
            fail_revocations.clone(),
            spend_next_registration.clone(),
            registration_ids.clone(),
        );
        let (login_tokens, device_tokens, refresh_tokens) =
            (tokens.clone(), tokens.clone(), tokens);
        let router = Router::new()
            .route(
                "/v1/auth/login",
                post(move |Json(body): Json<Value>| {
                    let (logins, tokens) = (logins.clone(), login_tokens.clone());
                    async move {
                        if body["password"] == WRONG_PASSWORD {
                            let error = json!({"error": {"code": "invalid_credentials", "message": "wrong email or password", "retryable": false}});
                            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
                        }
                        if body.get("device_id").is_some() {
                            let n = logins.fetch_add(1, Ordering::SeqCst) + 100;
                            Json(tokens(n)).into_response()
                        } else {
                            Json(json!({"registration_token": "registration", "expires_in_s": 300}))
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/devices",
                post(move |Json(body): Json<Value>| {
                    let (registered, spend, ids) = (registered.clone(), spend.clone(), ids.clone());
                    let tokens = device_tokens.clone();
                    async move {
                        let id = body["registration_id"].as_str().expect("a registration_id");
                        ids.lock().unwrap().push(id.to_owned());
                        if spend.swap(false, Ordering::SeqCst) {
                            let error = json!({"error": {"code": "device_not_found", "message": "no such device", "retryable": false}});
                            return (StatusCode::NOT_FOUND, Json(error)).into_response();
                        }
                        registered.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::CREATED, Json(tokens(0))).into_response()
                    }
                }),
            )
            .route(
                "/v1/devices/{id}",
                delete(move || {
                    let (revoked, fail) = (revoked.clone(), fail.clone());
                    async move {
                        if fail.load(Ordering::SeqCst) {
                            return StatusCode::INTERNAL_SERVER_ERROR;
                        }
                        revoked.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .route(
                "/v1/auth/refresh",
                post(move || {
                    let (refreshed, delay) = (refreshed.clone(), delay.clone());
                    let tokens = refresh_tokens.clone();
                    async move {
                        let n = refreshed.fetch_add(1, Ordering::SeqCst) + 1;
                        let delay = Duration::from_millis(delay.load(Ordering::SeqCst));
                        tokio::time::sleep(delay).await;
                        Json(tokens(n))
                    }
                }),
            )
            .route(
                "/v1/auth/signup",
                post(move || {
                    let signed_up = signed_up.clone();
                    async move {
                        signed_up.fetch_add(1, Ordering::SeqCst);
                        StatusCode::ACCEPTED
                    }
                }),
            )
            .route("/v1/auth/verify/resend", post(|| async { StatusCode::ACCEPTED }))
            .route("/v1/auth/password/forgot", post(|| async { StatusCode::ACCEPTED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            url,
            user,
            device,
            identity,
            registrations,
            refreshes,
            device_logins,
            revocations,
            refresh_delay_ms,
            fail_revocations,
            spend_next_registration,
            registration_ids,
            signups,
        }
    }
}

/// The invite code [`AutoGateway`] answers `invite_not_found` for; it redeems any other.
pub const UNKNOWN_INVITE: &str = "NOSUCHCODE";

/// A gateway that answers by itself, for tests that drive the SDK from outside Rust (the
/// bindings'): `Identify` gets `Ready` for the identity [`FakeAuth`] issues, `Ping` gets
/// `Pong`, every send, edit and delete is stored with the next `seq` of its conversation,
/// and every text message sent is answered by the peer with the same text, delivered next.
/// `OpenDirect`, `CreateInvite`, `RedeemInvite` and the listings are answered; any other
/// request is refused with `rejected`.
pub struct AutoGateway {
    pub url: String,
}

#[derive(Default)]
struct Log {
    /// Each conversation's peer and last `seq`.
    conversations: HashMap<ConversationId, (UserId, i64)>,
}

impl AutoGateway {
    pub async fn start(identity: Arc<Mutex<(UserId, DeviceId)>>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Log::default()));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (identity, log) = (identity.clone(), log.clone());
                tokio::spawn(async move {
                    if let Ok(socket) = accept_async(stream).await {
                        serve(socket, &identity, &log).await;
                    }
                });
            }
        });
        Self { url }
    }
}

fn now_us() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
}

fn id_bytes(id: Uuid) -> Vec<u8> {
    id.as_bytes().to_vec()
}

fn conversation_of(bytes: &[u8]) -> ConversationId {
    ConversationId(Uuid::from_slice(bytes).unwrap_or_default())
}

/// The next `seq` of `conversation`, and its peer (a stranger for one never opened).
fn append(log: &Mutex<Log>, conversation: ConversationId) -> (i64, UserId) {
    let mut log = log.lock().unwrap();
    let (peer, seq) = log
        .conversations
        .entry(conversation)
        .or_insert_with(|| (UserId::new(), 0));
    *seq += 1;
    (*seq, *peer)
}

fn ack(client_msg_id: Vec<u8>, seq: i64) -> Body {
    Body::SendAck(SendAck {
        client_msg_id,
        message_id: id_bytes(Uuid::now_v7()),
        seq,
        server_ts: now_us(),
    })
}

async fn serve(mut socket: Server, identity: &Mutex<(UserId, DeviceId)>, log: &Mutex<Log>) {
    while let Some(Ok(message)) = socket.next().await {
        let Message::Binary(bytes) = message else {
            continue;
        };
        let Ok(body) = decode_frame(&bytes) else {
            return;
        };
        let answers = match body {
            Body::Identify(_) => {
                let (user, device) = *identity.lock().unwrap();
                vec![Body::Ready(Ready {
                    user_id: id_bytes(user.0),
                    device_id: id_bytes(device.0),
                    heartbeat_ms: 30_000,
                })]
            }
            Body::Ping(_) => vec![Body::Pong(Pong {})],
            Body::Send(send) => {
                let conversation = conversation_of(&send.conversation_id);
                let (seq, _) = append(log, conversation);
                let (reply_seq, peer) = append(log, conversation);
                let reply = Deliver {
                    conversation_id: send.conversation_id,
                    seq: reply_seq,
                    message_id: id_bytes(Uuid::now_v7()),
                    client_msg_id: id_bytes(Uuid::now_v7()),
                    kind: EntryKind::Message.into(),
                    target_seq: None,
                    sender_id: id_bytes(peer.0),
                    content_type: ContentType::Text.into(),
                    body: Some(send.body),
                    client_ts: None,
                    server_ts: now_us(),
                    edited_at: None,
                    edit_seq: None,
                    deleted: false,
                };
                vec![ack(send.client_msg_id, seq), Body::Deliver(reply)]
            }
            Body::Edit(edit) => {
                let (seq, _) = append(log, conversation_of(&edit.conversation_id));
                vec![ack(edit.client_msg_id, seq)]
            }
            Body::Delete(delete) => {
                let (seq, _) = append(log, conversation_of(&delete.conversation_id));
                vec![ack(delete.client_msg_id, seq)]
            }
            Body::Request(request) => vec![answer(request, log)],
            _ => Vec::new(),
        };
        for answer in answers {
            if socket
                .send(Message::Binary(encode_frame(answer).into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

fn answer(request: Request, log: &Mutex<Log>) -> Body {
    let request_id = request.request_id;
    let refuse = |code: ErrorCode, message: &str| {
        Body::Error(push_proto::v1::Error {
            code: code.into(),
            message: message.to_owned(),
            retryable: false,
            request_id: Some(request_id),
            ..Default::default()
        })
    };
    let body = match request.body {
        Some(request::Body::OpenDirect(open)) => {
            let peer = UserId(Uuid::from_slice(&open.peer_user_id).unwrap_or_default());
            let mut log = log.lock().unwrap();
            let existing = log
                .conversations
                .iter()
                .find(|(_, (known, _))| *known == peer)
                .map(|(id, _)| *id);
            let id = existing.unwrap_or_else(|| {
                let id = ConversationId::new();
                log.conversations.insert(id, (peer, 0));
                id
            });
            response::Body::DirectOpened(DirectOpened {
                conversation_id: id_bytes(id.0),
            })
        }
        Some(request::Body::ListConversations(_)) => {
            response::Body::Conversations(ConversationList::default())
        }
        Some(request::Body::ListContacts(_)) => response::Body::Contacts(ContactList::default()),
        Some(request::Body::CreateInvite(create)) => {
            response::Body::Invite(push_proto::v1::Invite {
                code: "ABCDEFGHJK".to_owned(),
                expires_at: now_us() + i64::from(create.expires_in_s) * 1_000_000,
                max_uses: create.max_uses,
                uses: 0,
            })
        }
        Some(request::Body::RedeemInvite(redeem)) if redeem.code == UNKNOWN_INVITE => {
            return refuse(ErrorCode::InviteNotFound, "no such invite");
        }
        Some(request::Body::RedeemInvite(_)) => response::Body::InviteRedeemed(InviteRedeemed {
            user_id: id_bytes(Uuid::now_v7()),
        }),
        _ => return refuse(ErrorCode::Rejected, "the fake gateway does not answer this"),
    };
    Body::Response(Response {
        request_id,
        body: Some(body),
    })
}
