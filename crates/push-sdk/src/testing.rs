//! Test doubles shared by the connection's and the client's tests: a gateway driven frame
//! by frame, and auth-svc answering with counters.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, post};
use futures_util::{SinkExt, StreamExt};
use push_domain::{DeviceId, UserId};
use push_proto::v1::frame::Body;
use push_proto::{decode_frame, encode_frame};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, accept_async};

pub(crate) type Server = WebSocketStream<TcpStream>;

/// A gateway the test drives frame by frame: each accepted connection is handed over.
pub(crate) struct FakeGateway {
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
pub(crate) async fn recv(socket: &mut Server) -> Body {
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
pub(crate) async fn recv_until(socket: &mut Server, wanted: impl Fn(&Body) -> bool) -> Body {
    loop {
        let body = recv(socket).await;
        if wanted(&body) {
            return body;
        }
    }
}

pub(crate) async fn send(socket: &mut Server, body: Body) {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .unwrap();
}

pub(crate) async fn close(socket: &mut Server, code: u16) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    };
    let _ = socket.close(Some(frame)).await;
}

/// auth-svc for one user and device: sign-in, registration, refresh (answering
/// `access-N`) and revocation, each counted.
pub(crate) struct FakeAuth {
    pub url: String,
    pub user: UserId,
    pub device: DeviceId,
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
}

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
        let tokens = move |n: usize| {
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
        let router = Router::new()
            .route(
                "/v1/auth/login",
                post(move |Json(body): Json<Value>| {
                    let logins = logins.clone();
                    async move {
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
                    async move {
                        let n = refreshed.fetch_add(1, Ordering::SeqCst) + 1;
                        let delay = Duration::from_millis(delay.load(Ordering::SeqCst));
                        tokio::time::sleep(delay).await;
                        Json(tokens(n))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            url,
            user,
            device,
            registrations,
            refreshes,
            device_logins,
            revocations,
            refresh_delay_ms,
            fail_revocations,
            spend_next_registration,
            registration_ids,
        }
    }
}
