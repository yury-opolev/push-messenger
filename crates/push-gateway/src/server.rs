//! Running the gateway: the WebSocket endpoint, one task per connection, the token keys,
//! the health endpoints and graceful shutdown.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use prost::Message as _;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AckRequest, DeleteRequest, EditRequest, IdentifyRequest, Kick, KickReason, ReadUpToRequest,
    RequestCall, SendRequest, SyncRequest,
};
use push_proto::v1::frame::Body;
use push_proto::v1::{AuthExpiring, Error, ErrorCode, SendAck};
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes, kick_channel};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::config::Config;
use crate::hub::{Hub, Membership, Signal};
use crate::session::{RetryDelay, Session, Step, error_frame};
use crate::tokens::{Claims, TokenError, Verifier};

/// The first frame must be `Identify`, within this time (spec §7.1).
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for connections to close.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// `AuthExpiring` comes this long before the token expires (spec §7.3).
const EXPIRY_WARNING: Duration = Duration::from_secs(60);
/// A connection must send a valid `ReAuth` by this long after its token expired, or it is
/// closed with 4002 (spec §7.3).
const REAUTH_GRACE: Duration = Duration::from_secs(30);
/// How often the token keys are fetched again: auth-svc lets clients cache them 5 minutes.
const KEYS_REFRESH: Duration = Duration::from_secs(300);
/// How soon to fetch the keys again after a failed fetch.
const KEYS_RETRY: Duration = Duration::from_secs(2);
/// How long a Redis command (other than the subscriber's) may take.
const REDIS_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    messaging: MessagingClient<Channel>,
    verifier: Verifier,
    hub: Hub,
    /// Publishes takeovers on `kick:{device_id}`.
    redis: ConnectionManager,
    sessions: TaskTracker,
    shutdown: CancellationToken,
}

/// The connection was closed, or must be.
struct Closed;

/// Runs the gateway until `shutdown` is cancelled. Then it stops accepting connections,
/// closes every open one with 4005, and waits up to 10 s for them to finish.
pub async fn serve(
    config: Config,
    listener: TcpListener,
    health: TcpListener,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10));
    let messaging = MessagingClient::new(endpoint.connect_lazy());
    // The error is not passed on: the URL carries the Redis password.
    let redis = redis::Client::open(config.redis_url.as_str())
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let commands = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(REDIS_TIMEOUT))
        .set_response_timeout(Some(REDIS_TIMEOUT))
        .set_number_of_retries(1);
    let publisher = redis
        .get_connection_manager_lazy(commands)
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let (hub, subscriber) = Hub::start(redis, shutdown.clone());
    let verifier = Verifier::new(config.jwks_url(), &config.token_audience);
    let keys = tokio::spawn(refresh_keys(verifier.clone(), shutdown.clone()));
    let sessions = TaskTracker::new();
    let listen_addr = config.listen_addr;
    let app = App {
        config: Arc::new(config),
        messaging,
        verifier: verifier.clone(),
        hub: hub.clone(),
        redis: publisher,
        sessions: sessions.clone(),
        shutdown: shutdown.clone(),
    };

    let health_server = tokio::spawn(
        axum::serve(health, health_router(verifier, shutdown.clone()))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .into_future(),
    );
    let router = Router::new()
        .route("/v1/socket", get(upgrade))
        .with_state(app);
    tracing::info!(addr = %listen_addr, "serving WebSockets");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await?;

    // Upgraded connections outlive the HTTP server; each closes itself with 4005.
    sessions.close();
    if tokio::time::timeout(DRAIN_TIMEOUT, sessions.wait())
        .await
        .is_err()
    {
        tracing::warn!(open = sessions.len(), "connections still open after 10 s");
    }
    subscriber.await?;
    keys.await?;
    health_server.await??;
    tracing::info!("stopped");
    Ok(())
}

/// Keeps the token keys fresh: fetches them every 2 s until it has them, then every
/// 5 minutes (a token naming a new key fetches them at once, see [`Verifier`]). A failed
/// fetch keeps the keys it had.
async fn refresh_keys(verifier: Verifier, shutdown: CancellationToken) {
    loop {
        let fetched = tokio::select! {
            () = shutdown.cancelled() => return,
            fetched = verifier.fetch() => fetched,
        };
        let wait = match fetched {
            Ok(()) => KEYS_REFRESH,
            Err(error) => {
                tracing::warn!(%error, "could not fetch the token keys; retrying");
                if verifier.is_ready() {
                    KEYS_REFRESH / 10
                } else {
                    KEYS_RETRY
                }
            }
        };
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// `/healthz` always answers; `/readyz` answers once the token keys are loaded, until
/// shutdown begins. Readiness does not depend on Redis: without it devices still get
/// their entries, only later (after `Ready`, after acks and on retries), while an unready
/// gateway would refuse every client. The subscriber reconnects by itself, so liveness
/// does not depend on it either.
fn health_router(verifier: Verifier, shutdown: CancellationToken) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || async move {
                if shutdown.is_cancelled() {
                    (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
                } else if !verifier.is_ready() {
                    (StatusCode::SERVICE_UNAVAILABLE, "loading the token keys")
                } else {
                    (StatusCode::OK, "ready")
                }
            }),
        )
}

/// A message is at most [`MAX_FRAME_BYTES`]. A WebSocket frame may be bigger, up to
/// 512 KiB, so that a message over the limit is read whole and refused with a close frame
/// (1009) the client can read; a frame above 512 KiB is refused from its header, before
/// its payload is buffered, and the client may only see the connection drop.
const MAX_WEBSOCKET_FRAME: usize = 4 * MAX_FRAME_BYTES;

async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    let sessions = app.sessions.clone();
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_WEBSOCKET_FRAME)
        .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
}

/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new(app.config.groups);
    let Some((claims, mut membership)) = identify(&mut socket, &mut session, &app).await else {
        return;
    };
    membership.join_user(claims.user).await;
    let Some(ready) = session.identified(&claims, app.config.heartbeat) else {
        return;
    };
    take_over(&app, claims.device, membership.connection()).await;
    let mut connected = Connected {
        socket,
        session,
        messaging: app.messaging.clone(),
        verifier: app.verifier.clone(),
        user: claims.user,
        device: claims.device,
        expiry: Expiry::new(claims.expires_at),
        issued_at: claims.issued_at,
        retry: RetryDelay::default(),
        retry_at: None,
    };
    if send(&mut connected.socket, ready).await.is_err() || connected.deliver().await.is_err() {
        return;
    }
    loop {
        let retry_at = connected.retry_at;
        let (warn_at, warned, deadline) = (
            connected.expiry.warn_at,
            connected.expiry.warned,
            connected.expiry.deadline,
        );
        tokio::select! {
            () = app.shutdown.cancelled() => {
                close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
                return;
            }
            signal = membership.next() => {
                let done = match signal {
                    Signal::Ring => connected.deliver().await,
                    Signal::Frame(frame) => send(&mut connected.socket, *frame).await,
                    Signal::Kick(reason) => {
                        let (code, why) = kick_close(reason);
                        tracing::info!(device = %connected.device, ?reason, "connection kicked");
                        close_with(&mut connected.socket, code, why).await;
                        Err(Closed)
                    }
                };
                if done.is_err() {
                    return;
                }
            }
            // The precondition disables the branch; the far-away instant is never awaited.
            () = tokio::time::sleep_until(retry_at.unwrap_or_else(far_future)), if retry_at.is_some() => {
                if connected.deliver().await.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(warn_at), if !warned => {
                if connected.warn_expiry().await.is_err() {
                    return;
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                close_with(&mut connected.socket, close::REAUTH_REQUIRED, "the access token expired; send ReAuth in time").await;
                return;
            }
            message = connected.socket.recv() => {
                let step = match message {
                    Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
                    Some(Ok(Message::Text(_))) => connected.session.on_text(),
                    // WebSocket pings are answered by the library.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Err(error)) => {
                        refuse_oversized(&mut connected.socket, error).await;
                        return;
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                };
                if connected.run(step).await.is_err() {
                    return;
                }
                membership.renewed(connected.issued_at);
            }
        }
    }
}

/// A message over the WebSocket limit ([`MAX_FRAME_BYTES`]) ends the connection with 1009
/// (RFC 6455: message too big); other read errors end it without a word, the peer being
/// gone or broken.
async fn refuse_oversized(socket: &mut WebSocket, error: axum::Error) {
    let oversized = error
        .into_inner()
        .downcast::<tokio_tungstenite::tungstenite::Error>()
        .is_ok_and(|error| matches!(*error, tokio_tungstenite::tungstenite::Error::Capacity(_)));
    if oversized {
        close_with(socket, close::MESSAGE_TOO_BIG, "a frame is at most 128 KiB").await;
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
}

/// Waits for the first frame, checks its token and asks message-svc whether the device it
/// names may connect. The connection watches the device's kicks before message-svc
/// answers, so a revocation or password reset committed after the answer still reaches
/// it. `None` once the connection is closed.
async fn identify(
    socket: &mut WebSocket,
    session: &mut Session,
    app: &App,
) -> Option<(Claims, Membership)> {
    let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
    tokio::pin!(deadline);
    let step = loop {
        tokio::select! {
            () = &mut deadline => {
                close_with(socket, close::PROTOCOL_ERROR, "Identify must come within 5 s").await;
                return None;
            }
            () = app.shutdown.cancelled() => {
                close_with(socket, close::SHUTTING_DOWN, "server shutting down").await;
                return None;
            }
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => break session.on_binary(&bytes),
                Some(Ok(Message::Text(_))) => break session.on_text(),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Err(error)) => {
                    refuse_oversized(socket, error).await;
                    return None;
                }
                Some(Ok(Message::Close(_))) | None => return None,
            },
        }
    };
    let token = match step {
        Step::Identify(token) => token,
        Step::Close(code, reason) => {
            close_with(socket, code, reason).await;
            return None;
        }
        // Before Ready the session only asks to identify or to close.
        _ => {
            close_with(
                socket,
                close::PROTOCOL_ERROR,
                "the first frame must be Identify",
            )
            .await;
            return None;
        }
    };
    let claims = match verify(&app.verifier, &token).await {
        Ok(claims) => claims,
        Err(error) => {
            let (code, reason) = token_refusal(error);
            close_with(socket, code, reason).await;
            return None;
        }
    };
    // Taken now, not when the socket opened: a connection that starts watching later is the
    // newer one, so its takeover wins even if it opened first and its token check stalled.
    let connection_id = Uuid::now_v7();
    let membership = app
        .hub
        .watch_device(claims.device, connection_id, claims.issued_at)
        .await;
    match admit(&mut app.messaging.clone(), &claims).await {
        Ok(()) => {
            tracing::info!(user = %claims.user, device = %claims.device, "device connected");
            Some((claims, membership))
        }
        Err(refusal) => {
            let (code, reason) = refusal.close();
            close_with(socket, code, reason).await;
            None
        }
    }
}

/// The claims of `token` if it passes and its 4002 deadline (`exp` + [`REAUTH_GRACE`]) is
/// still ahead. A token past the deadline but inside the leeway would be admitted only to
/// be closed with 4002 at once, so it is `Expired` here.
async fn verify(verifier: &Verifier, token: &str) -> Result<Claims, TokenError> {
    let claims = verifier.verify(token).await?;
    if unix_now() >= claims.expires_at + REAUTH_GRACE.as_secs() as i64 {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

/// Tells any other connection of the device, on any gateway, to close: the newest
/// connection wins (spec §7.3). Best effort: without Redis two connections may coexist
/// for a while, which the client tolerates.
async fn take_over(app: &App, device: Uuid, connection_id: Uuid) {
    let kick = Kick {
        reason: KickReason::TakenOver.into(),
        connection_id: id_bytes(connection_id),
        ..Kick::default()
    };
    let mut redis = app.redis.clone();
    let mut command = redis::cmd("PUBLISH");
    command.arg(kick_channel(device)).arg(kick.encode_to_vec());
    let publish = command.exec_async(&mut redis);
    tokio::select! {
        () = app.shutdown.cancelled() => {}
        published = tokio::time::timeout(REDIS_TIMEOUT, publish) => {
            if !matches!(published, Ok(Ok(()))) {
                tracing::warn!(%device, "could not announce the connection to other gateways");
            }
        }
    }
}

/// The close code and reason for a kick.
fn kick_close(reason: KickReason) -> (u16, &'static str) {
    match reason {
        KickReason::PasswordReset => (
            close::INVALID_IDENTITY,
            "the password was changed; sign in again",
        ),
        KickReason::DeviceRevoked => (close::TAKEN_OVER, "the device was revoked"),
        KickReason::TakenOver | KickReason::Unspecified => (
            close::TAKEN_OVER,
            "another connection of this device took over",
        ),
    }
}

/// The close code and reason for a token that did not pass.
fn token_refusal(error: TokenError) -> (u16, &'static str) {
    match error {
        TokenError::Invalid => (close::INVALID_IDENTITY, "the access token is not valid"),
        TokenError::Expired => (close::REAUTH_REQUIRED, "the access token has expired"),
        TokenError::KeysUnavailable => (
            close::INTERNAL_ERROR,
            "access tokens cannot be checked now; retry",
        ),
    }
}

/// Why message-svc did not admit a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Close with this code and reason.
    Close(u16, &'static str),
    /// message-svc could not answer: retry later.
    Unavailable,
}

impl Refusal {
    fn close(self) -> (u16, &'static str) {
        match self {
            Self::Close(code, reason) => (code, reason),
            Self::Unavailable => (close::INTERNAL_ERROR, "the message service is unavailable"),
        }
    }
}

/// Asks message-svc whether the device of a valid token may connect: it is active and the
/// user's, and the token is newer than the user's last password reset.
async fn admit(messaging: &mut MessagingClient<Channel>, claims: &Claims) -> Result<(), Refusal> {
    let request = IdentifyRequest {
        user_id: id_bytes(claims.user),
        device_id: id_bytes(claims.device),
        issued_at: claims.issued_at,
    };
    let Err(status) = messaging.identify(request).await else {
        return Ok(());
    };
    match error_from_status(&status).code() {
        ErrorCode::PasswordChanged => Err(Refusal::Close(
            close::INVALID_IDENTITY,
            "the password was changed; sign in again",
        )),
        ErrorCode::UnknownDevice => {
            Err(Refusal::Close(close::TAKEN_OVER, "the device was revoked"))
        }
        ErrorCode::DeviceNotReady => {
            // Normal for a second after a device registers: the worker has not relayed it.
            tracing::debug!(device = %claims.device, "device not relayed yet");
            Err(Refusal::Close(
                close::NOT_READY_YET,
                "the device is not ready yet; retry shortly",
            ))
        }
        _ => {
            tracing::warn!(code = ?status.code(), message = status.message(), "identify failed");
            Err(Refusal::Unavailable)
        }
    }
}

/// When the connection's token expires, and what to do about it.
#[derive(Debug, Clone, Copy)]
struct Expiry {
    /// The token's `exp`, in seconds since the Unix epoch.
    expires_at: i64,
    /// When to send `AuthExpiring`.
    warn_at: Instant,
    warned: bool,
    /// When to close with 4002 unless a `ReAuth` came.
    deadline: Instant,
}

impl Expiry {
    fn new(expires_at: i64) -> Self {
        let left = expires_at - unix_now();
        let at = |offset: i64| {
            Instant::now() + Duration::from_secs(u64::try_from(left + offset).unwrap_or(0))
        };
        Self {
            expires_at,
            warn_at: at(-(EXPIRY_WARNING.as_secs() as i64)),
            warned: false,
            deadline: at(REAUTH_GRACE.as_secs() as i64),
        }
    }

    /// Seconds until the token expires; 0 once it has.
    fn expires_in_s(&self) -> u32 {
        u32::try_from((self.expires_at - unix_now()).max(0)).unwrap_or(u32::MAX)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

/// An identified connection.
struct Connected {
    socket: WebSocket,
    session: Session,
    messaging: MessagingClient<Channel>,
    verifier: Verifier,
    user: Uuid,
    device: Uuid,
    expiry: Expiry,
    /// The `iat` of the current token: a password-reset kick spares a newer one.
    issued_at: i64,
    retry: RetryDelay,
    /// When to retry a failed sync; `None` while the last sync succeeded.
    retry_at: Option<Instant>,
}

impl Connected {
    /// Does what the session asked for a client frame.
    async fn run(&mut self, step: Step) -> Result<(), Closed> {
        match step {
            Step::Reply(body) => send(&mut self.socket, body).await,
            Step::ReAuth(token) => self.reauth(&token).await,
            Step::Send(frame) => {
                let client_msg_id = frame.client_msg_id.clone();
                let request = SendRequest {
                    user_id: id_bytes(self.user),
                    device_id: id_bytes(self.device),
                    send: Some(frame),
                };
                let body = match self.messaging.send(request).await {
                    Ok(ack) => Body::SendAck(ack.into_inner()),
                    Err(status) => {
                        let mut error = error_from_status(&status);
                        error.client_msg_id = Some(client_msg_id);
                        Body::Error(error)
                    }
                };
                send(&mut self.socket, body).await
            }
            Step::Ack(ack) => {
                let request = AckRequest {
                    device_id: id_bytes(self.device),
                    ack: Some(ack),
                };
                match self.messaging.ack(request).await {
                    // An ack rings nobody: if the last sync left entries behind, this is
                    // the moment to fetch them.
                    Ok(response) if self.session.sync_after_ack(response.get_ref().moved) => {
                        self.deliver().await
                    }
                    Ok(_) => Ok(()),
                    Err(status) => {
                        let error = Body::Error(error_from_status(&status));
                        send(&mut self.socket, error).await
                    }
                }
            }
            Step::Edit(edit) => {
                let client_msg_id = edit.client_msg_id.clone();
                let request = EditRequest {
                    user_id: id_bytes(self.user),
                    device_id: id_bytes(self.device),
                    edit: Some(edit),
                };
                let answer = self.messaging.edit(request).await;
                self.acknowledge(answer, client_msg_id).await
            }
            Step::Delete(delete) => {
                let client_msg_id = delete.client_msg_id.clone();
                let request = DeleteRequest {
                    user_id: id_bytes(self.user),
                    device_id: id_bytes(self.device),
                    delete: Some(delete),
                };
                let answer = self.messaging.delete(request).await;
                self.acknowledge(answer, client_msg_id).await
            }
            Step::ReadUpTo(read) => {
                let request = ReadUpToRequest {
                    user_id: id_bytes(self.user),
                    read: Some(read),
                };
                match self.messaging.read_up_to(request).await {
                    Ok(_) => Ok(()),
                    Err(status) => {
                        let error = Body::Error(error_from_status(&status));
                        send(&mut self.socket, error).await
                    }
                }
            }
            Step::Request(request) => {
                let request_id = request.request_id;
                let call = RequestCall {
                    user_id: id_bytes(self.user),
                    request: Some(request),
                };
                let body = match self.messaging.request(call).await {
                    Ok(response) => Body::Response(response.into_inner()),
                    Err(status) => {
                        let mut error = error_from_status(&status);
                        error.request_id = Some(request_id);
                        Body::Error(error)
                    }
                };
                send(&mut self.socket, body).await
            }
            Step::Close(code, reason) => {
                close_with(&mut self.socket, code, reason).await;
                Err(Closed)
            }
            Step::Identify(_) => {
                let error = error_frame(ErrorCode::BadFrame, "already identified");
                send(&mut self.socket, error).await
            }
        }
    }

    /// Answers an edit or a delete: its `SendAck`, or the error with its `client_msg_id`.
    async fn acknowledge(
        &mut self,
        answer: Result<tonic::Response<SendAck>, tonic::Status>,
        client_msg_id: Vec<u8>,
    ) -> Result<(), Closed> {
        let body = match answer {
            Ok(ack) => Body::SendAck(ack.into_inner()),
            Err(status) => {
                let mut error = error_from_status(&status);
                error.client_msg_id = Some(client_msg_id);
                Body::Error(error)
            }
        };
        send(&mut self.socket, body).await
    }

    /// Tells the client its token expires soon, once per token.
    async fn warn_expiry(&mut self) -> Result<(), Closed> {
        self.expiry.warned = true;
        let warning = AuthExpiring {
            expires_in_s: self.expiry.expires_in_s(),
        };
        send(&mut self.socket, Body::AuthExpiring(warning)).await
    }

    /// A fresh token for the connection: it must pass, name the same user and device, and
    /// be newer than the user's last password reset. Nothing answers a good one. When the
    /// keys or message-svc cannot be reached, a retryable error asks the client to send it
    /// again; the 4002 deadline still applies.
    async fn reauth(&mut self, token: &str) -> Result<(), Closed> {
        let claims = match verify(&self.verifier, token).await {
            Ok(claims) => claims,
            Err(TokenError::KeysUnavailable) => return self.retry_reauth().await,
            Err(error) => {
                let (code, reason) = token_refusal(error);
                close_with(&mut self.socket, code, reason).await;
                return Err(Closed);
            }
        };
        if claims.user != self.user || claims.device != self.device {
            close_with(
                &mut self.socket,
                close::INVALID_IDENTITY,
                "the token names another device",
            )
            .await;
            return Err(Closed);
        }
        match admit(&mut self.messaging, &claims).await {
            Ok(()) => {
                self.expiry = Expiry::new(claims.expires_at);
                self.issued_at = claims.issued_at;
                Ok(())
            }
            Err(Refusal::Unavailable) => self.retry_reauth().await,
            Err(Refusal::Close(code, reason)) => {
                close_with(&mut self.socket, code, reason).await;
                Err(Closed)
            }
        }
    }

    async fn retry_reauth(&mut self) -> Result<(), Closed> {
        let error = Error {
            code: ErrorCode::Unavailable.into(),
            message: "the token could not be checked now; send ReAuth again".to_owned(),
            retryable: true,
            client_msg_id: None,
            request_id: None,
            retry_after_s: Some(1),
        };
        send(&mut self.socket, Body::Error(error)).await
    }

    /// Pushes every entry the device has not been sent yet. A failed sync is retried
    /// after [`RetryDelay`], from the connection's loop.
    async fn deliver(&mut self) -> Result<(), Closed> {
        loop {
            let request = SyncRequest {
                device_id: id_bytes(self.device),
                limit: 0,
            };
            let batch = match self.messaging.sync(request).await {
                Ok(batch) => batch.into_inner(),
                Err(status) => {
                    let delay = self.retry.failed();
                    self.retry_at = Some(Instant::now() + delay);
                    tracing::warn!(
                        code = ?status.code(),
                        message = status.message(),
                        retry_in = ?delay,
                        "sync failed"
                    );
                    return Ok(());
                }
            };
            self.retry.succeeded();
            self.retry_at = None;
            self.session.synced(batch.more);
            let fresh = self.session.fresh(batch.entries);
            let progressed = !fresh.is_empty();
            for entry in fresh {
                send(&mut self.socket, Body::Deliver(entry)).await?;
            }
            // Without acks a repeated sync returns the same batch: stop here. The ack that
            // moves the position syncs again (`Session::sync_after_ack`).
            if !(batch.more && progressed) {
                return Ok(());
            }
        }
    }
}

async fn send(socket: &mut WebSocket, body: Body) -> Result<(), Closed> {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .map_err(|_| Closed)
}

async fn close_with(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

/// Structured JSON logs on stdout. The level comes from `RUST_LOG` (default `info`).
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .init();
}

/// Resolves on Ctrl-C, or on SIGTERM where there is one (Kubernetes sends it).
pub async fn terminated() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}
