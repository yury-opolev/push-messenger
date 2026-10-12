//! Running the gateway: the WebSocket endpoint, one task per connection, the health
//! endpoints and graceful shutdown.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{AckRequest, IdentifyRequest, SendRequest, SyncRequest};
use push_proto::v1::ErrorCode;
use push_proto::v1::frame::Body;
use push_proto::{MAX_FRAME_BYTES, close, encode_frame, error_from_status, id_bytes};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::config::Config;
use crate::hub::Hub;
use crate::session::{RetryDelay, Session, Step, error_frame};

/// The first frame must be `Identify`, within this time (spec §7.1).
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for connections to close.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Close code for "the server cannot serve this connection now" (RFC 6455).
const INTERNAL_ERROR: u16 = 1011;

#[derive(Clone)]
struct App {
    config: Arc<Config>,
    messaging: MessagingClient<Channel>,
    hub: Hub,
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
    if config.dev_auth {
        tracing::warn!(
            "DEV ONLY: PUSH_DEV_AUTH=1 accepts any user and device id in Identify without a \
             token. Anyone who can reach this gateway can act as any user. Never enable it \
             outside a local cluster."
        );
    }
    let endpoint = Endpoint::from_shared(config.message_svc_url.clone())?
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10));
    let messaging = MessagingClient::new(endpoint.connect_lazy());
    // The error is not passed on: the URL carries the Redis password.
    let redis = redis::Client::open(config.redis_url.as_str())
        .map_err(|_| anyhow::anyhow!("PUSH_REDIS_URL is not a valid Redis URL"))?;
    let (hub, subscriber) = Hub::start(redis, shutdown.clone());
    let sessions = TaskTracker::new();
    let listen_addr = config.listen_addr;
    let app = App {
        config: Arc::new(config),
        messaging,
        hub: hub.clone(),
        sessions: sessions.clone(),
        shutdown: shutdown.clone(),
    };

    let health_server = tokio::spawn(
        axum::serve(health, health_router(shutdown.clone()))
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
    health_server.await??;
    tracing::info!("stopped");
    Ok(())
}

/// `/healthz` always answers; `/readyz` answers until shutdown begins. Readiness does not
/// depend on Redis: without it devices still get their entries, only later (after
/// `Ready`, after acks and on retries), while an unready gateway would refuse every
/// client. The subscriber reconnects by itself, so liveness does not depend on it either.
fn health_router(shutdown: CancellationToken) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || async move {
                if shutdown.is_cancelled() {
                    (StatusCode::SERVICE_UNAVAILABLE, "shutting down")
                } else {
                    (StatusCode::OK, "ready")
                }
            }),
        )
}

async fn upgrade(upgrade: WebSocketUpgrade, State(app): State<App>) -> Response {
    let sessions = app.sessions.clone();
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| sessions.track_future(connection(socket, app)))
}

/// One client connection, from `Identify` to close.
async fn connection(mut socket: WebSocket, app: App) {
    let mut session = Session::new(app.config.dev_auth);
    let Some((user, device)) = identify(&mut socket, &mut session, &app).await else {
        return;
    };
    let mut membership = app.hub.join(user).await;
    let Some(ready) = session.identified(app.config.heartbeat) else {
        return;
    };
    let mut connected = Connected {
        socket,
        session,
        messaging: app.messaging.clone(),
        user,
        device,
        retry: RetryDelay::default(),
        retry_at: None,
    };
    if send(&mut connected.socket, ready).await.is_err() || connected.deliver().await.is_err() {
        return;
    }
    loop {
        let retry_at = connected.retry_at;
        tokio::select! {
            () = app.shutdown.cancelled() => {
                close_with(&mut connected.socket, close::SHUTTING_DOWN, "server shutting down").await;
                return;
            }
            () = membership.rung() => {
                if connected.deliver().await.is_err() {
                    return;
                }
            }
            // The precondition disables the branch; the far-away instant is never awaited.
            () = tokio::time::sleep_until(retry_at.unwrap_or_else(far_future)), if retry_at.is_some() => {
                if connected.deliver().await.is_err() {
                    return;
                }
            }
            message = connected.socket.recv() => {
                let step = match message {
                    Some(Ok(Message::Binary(bytes))) => connected.session.on_binary(&bytes),
                    Some(Ok(Message::Text(_))) => connected.session.on_text(),
                    // WebSocket pings are answered by the library.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                };
                if connected.run(step).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
}

/// Waits for the first frame and checks the identity with message-svc. `None` once the
/// connection is closed.
async fn identify(
    socket: &mut WebSocket,
    session: &mut Session,
    app: &App,
) -> Option<(Uuid, Uuid)> {
    let deadline = tokio::time::sleep(IDENTIFY_TIMEOUT);
    tokio::pin!(deadline);
    let step = loop {
        tokio::select! {
            () = &mut deadline => {
                close_with(socket, close::INVALID_IDENTITY, "Identify must come within 5 s").await;
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
                Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            },
        }
    };
    let (user, device) = match step {
        Step::Identify { user, device } => (user, device),
        Step::Close(code, reason) => {
            close_with(socket, code, reason).await;
            return None;
        }
        // Before Ready the session only asks to identify or to close.
        Step::Reply(_) | Step::Send(_) | Step::Ack(_) => {
            close_with(
                socket,
                close::INVALID_IDENTITY,
                "the first frame must be Identify",
            )
            .await;
            return None;
        }
    };
    let request = IdentifyRequest {
        user_id: id_bytes(user),
        device_id: id_bytes(device),
    };
    match app.messaging.clone().identify(request).await {
        Ok(_) => {
            tracing::info!(%user, %device, dev_identity = true, "device connected");
            Some((user, device))
        }
        Err(status)
            if matches!(
                status.code(),
                tonic::Code::PermissionDenied | tonic::Code::InvalidArgument
            ) =>
        {
            close_with(socket, close::INVALID_IDENTITY, "unknown or revoked device").await;
            None
        }
        Err(status) => {
            tracing::warn!(code = ?status.code(), message = status.message(), "identify failed");
            close_with(socket, INTERNAL_ERROR, "the message service is unavailable").await;
            None
        }
    }
}

/// An identified connection.
struct Connected {
    socket: WebSocket,
    session: Session,
    messaging: MessagingClient<Channel>,
    user: Uuid,
    device: Uuid,
    retry: RetryDelay,
    /// When to retry a failed sync; `None` while the last sync succeeded.
    retry_at: Option<Instant>,
}

impl Connected {
    /// Does what the session asked for a client frame.
    async fn run(&mut self, step: Step) -> Result<(), Closed> {
        match step {
            Step::Reply(body) => send(&mut self.socket, body).await,
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
            Step::Close(code, reason) => {
                close_with(&mut self.socket, code, reason).await;
                Err(Closed)
            }
            Step::Identify { .. } => {
                let error = error_frame(ErrorCode::BadFrame, "already identified");
                send(&mut self.socket, error).await
            }
        }
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
