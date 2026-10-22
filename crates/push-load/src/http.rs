//! auth-svc's public API as push-load uses it: sign-in, device registration and
//! refresh. Errors never carry URLs, bodies or tokens. A 429 or 503 pauses every worker
//! of the process for its `Retry-After`, so the harness respects the target's limits.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::config::Target;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(900);
const TRANSPORT_ATTEMPTS: u32 = 5;
const TRANSPORT_PAUSE: Duration = Duration::from_secs(2);
/// Every attempt to refresh one token ends within this, inside auth-svc's 30 s grace for
/// presenting a just-rotated token again (as the SDK's `REFRESH_WINDOW`).
pub const REFRESH_WINDOW: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 429 or 503: the same request may succeed after this long.
    RetryAfter(Duration),
    /// Any other unexpected status, with auth-svc's stable error code.
    Rejected { status: u16, code: String },
    /// The request did not complete.
    Transport(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RetryAfter(delay) => write!(formatter, "retry after {} s", delay.as_secs()),
            Self::Rejected { status, code } => write!(formatter, "{code} (HTTP {status})"),
            Self::Transport(error) => write!(formatter, "the request failed: {error}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// One device's sign-in, as auth-svc answers it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceTokens {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in_s: u64,
}

impl std::fmt::Debug for DeviceTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceTokens")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// The `Retry-After` of a refusal, between 1 s and 15 minutes.
pub fn retry_delay(header: Option<&str>) -> Duration {
    header
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(1), Duration::from_secs)
        .clamp(Duration::from_secs(1), MAX_RETRY_AFTER)
}

/// When the next request may go: shared by every worker of the process.
#[derive(Debug, Clone, Default)]
pub struct Pacer(Arc<Mutex<Option<Instant>>>);

impl Pacer {
    pub async fn wait(&self) {
        let until = *self.0.lock().await;
        if let Some(until) = until {
            tokio::time::sleep_until(until).await;
        }
    }

    /// Waits for the pause to end if it ends by `deadline`; false (at once) if it does not.
    pub async fn wait_within(&self, deadline: Instant) -> bool {
        let until = *self.0.lock().await;
        match until {
            Some(until) if until > deadline => false,
            Some(until) => {
                tokio::time::sleep_until(until).await;
                true
            }
            None => true,
        }
    }

    pub async fn defer(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut next = self.0.lock().await;
        if next.is_none_or(|current| current < until) {
            *next = Some(until);
            eprintln!(
                "push-load: the target asked to slow down; pausing {} s",
                delay.as_secs()
            );
        }
    }
}

#[derive(Clone)]
pub struct AuthApi {
    http: Client,
    target: Target,
    pacer: Pacer,
}

impl AuthApi {
    pub fn new(target: &Target) -> anyhow::Result<Self> {
        let mut builder = Client::builder().timeout(REQUEST_TIMEOUT).no_proxy();
        if let Some(ip) = target.connect_to() {
            builder = builder.resolve(target.host(), std::net::SocketAddr::new(ip, target.port()));
        }
        Ok(Self {
            http: builder.build().context("build the HTTP client")?,
            target: target.clone(),
            pacer: Pacer::default(),
        })
    }

    async fn post_once(
        &self,
        path: &str,
        body: &Value,
        bearer: Option<&str>,
        expected: StatusCode,
    ) -> Result<Value, ApiError> {
        let mut request = self.http.post(self.target.api_url(path)).json(body);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| ApiError::Transport(error.without_url().to_string()))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        if status == expected {
            return Ok(answer);
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            return Err(ApiError::RetryAfter(retry_delay(retry_after.as_deref())));
        }
        // A server/proxy may echo credentials even in its code field.
        let code = answer["error"]["code"]
            .as_str()
            .filter(|code| {
                matches!(
                    *code,
                    "invalid"
                        | "handle_taken"
                        | "invalid_credentials"
                        | "email_not_verified"
                        | "link_invalid"
                        | "link_expired"
                        | "unauthorized"
                        | "token_expired"
                        | "too_many_devices"
                        | "device_not_found"
                        | "user_not_found"
                        | "entra_token_invalid"
                        | "entra_unavailable"
                        | "token_invalid"
                        | "identity_unavailable"
                        | "sign_in_not_permitted"
                        | "session_expired"
                        | "rate_limited"
                        | "unavailable"
                )
            })
            .unwrap_or("unknown")
            .to_owned();
        Err(ApiError::Rejected {
            status: status.as_u16(),
            code,
        })
    }

    /// Waits out rate limits (pausing every worker) and retries up to five transport
    /// failures; every request here is safe to repeat (registration is idempotent by
    /// `registration_id`). With a `window`, every attempt and wait ends within it: a
    /// repeated refresh is only safe inside auth-svc's 30 s grace after a rotation.
    async fn post(
        &self,
        path: &str,
        body: Value,
        bearer: Option<&str>,
        expected: StatusCode,
        window: Option<Duration>,
    ) -> Result<Value, ApiError> {
        let deadline = window.map(|window| Instant::now() + window);
        let expired =
            || ApiError::Transport("the refresh window ended without an answer".to_owned());
        let mut failures = 0;
        loop {
            let attempt = match deadline {
                None => {
                    self.pacer.wait().await;
                    self.post_once(path, &body, bearer, expected).await
                }
                Some(deadline) => {
                    if !self.pacer.wait_within(deadline).await {
                        return Err(expired());
                    }
                    tokio::time::timeout_at(deadline, self.post_once(path, &body, bearer, expected))
                        .await
                        .unwrap_or_else(|_| Err(expired()))
                }
            };
            match attempt {
                Err(ApiError::RetryAfter(delay)) => self.pacer.defer(delay).await,
                Err(ApiError::Transport(error)) => {
                    failures += 1;
                    if failures >= TRANSPORT_ATTEMPTS
                        || deadline
                            .is_some_and(|deadline| Instant::now() + TRANSPORT_PAUSE >= deadline)
                    {
                        return Err(ApiError::Transport(error));
                    }
                    tokio::time::sleep(TRANSPORT_PAUSE).await;
                }
                other => return other,
            }
        }
    }

    /// A registration token for the account.
    pub async fn login(&self, email: &str, password: &str) -> Result<String, ApiError> {
        let answer = self
            .post(
                "/v1/auth/login",
                json!({"email": email, "password": password}),
                None,
                StatusCode::OK,
                None,
            )
            .await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(malformed)
    }

    async fn authenticate(
        &self,
        account: &crate::accounts::Account,
        device: Option<Uuid>,
    ) -> Result<Value, ApiError> {
        let (path, mut body) = match account.entra_token().map_err(|_| ApiError::Rejected {
            status: 400,
            code: "entra_token_source_unreadable_or_changed_identity".to_owned(),
        })? {
            Some(token) => ("/v1/auth/entra", json!({"token":token})),
            None => (
                "/v1/auth/login",
                json!({"email":account.email,"password":account.password}),
            ),
        };
        if let Some(device) = device {
            body["device_id"] = json!(device);
        }
        self.post(path, body, None, StatusCode::OK, None).await
    }

    pub async fn login_account(
        &self,
        account: &crate::accounts::Account,
    ) -> Result<String, ApiError> {
        self.authenticate(account, None).await?["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(malformed)
    }

    pub async fn login_account_device(
        &self,
        account: &crate::accounts::Account,
        device: Uuid,
    ) -> Result<DeviceTokens, ApiError> {
        tokens(self.authenticate(account, Some(device)).await?)
    }

    /// Signs a known device in again (its tokens were lost or refused).
    pub async fn login_device(
        &self,
        email: &str,
        password: &str,
        device_id: Uuid,
    ) -> Result<DeviceTokens, ApiError> {
        let body = json!({"email": email, "password": password, "device_id": device_id});
        tokens(
            self.post("/v1/auth/login", body, None, StatusCode::OK, None)
                .await?,
        )
    }

    pub async fn register_device(
        &self,
        registration_token: &str,
        registration_id: Uuid,
        name: &str,
    ) -> Result<DeviceTokens, ApiError> {
        let body = json!({"name": name, "platform": "other", "registration_id": registration_id});
        tokens(
            self.post(
                "/v1/devices",
                body,
                Some(registration_token),
                StatusCode::CREATED,
                None,
            )
            .await?,
        )
    }

    /// Rotates the device's tokens. Retries stay within [`REFRESH_WINDOW`]: presenting the
    /// old token after auth-svc's 30 s grace would revoke the device's tokens.
    pub async fn refresh(&self, refresh_token: &str) -> Result<DeviceTokens, ApiError> {
        let body = json!({"refresh_token": refresh_token});
        tokens(
            self.post(
                "/v1/auth/refresh",
                body,
                None,
                StatusCode::OK,
                Some(REFRESH_WINDOW),
            )
            .await?,
        )
    }
}

fn malformed() -> ApiError {
    ApiError::Rejected {
        status: 200,
        code: "malformed_answer".to_owned(),
    }
}

#[cfg(test)]
mod safe_error_tests {
    use super::*;

    #[tokio::test]
    async fn http_errors_never_echo_server_supplied_credentials() {
        let router = axum::Router::new().route(
            "/v1/auth/login",
            axum::routing::post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "error": {
                            "code": "operator-token-secret",
                            "message": "operator-token-secret"
                        }
                    })),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(axum::serve(listener, router).into_future());
        let target = Target::new(&format!("http://{authority}"), &authority, None).unwrap();
        let error = AuthApi::new(&target)
            .unwrap()
            .login("a@x.test", "operator-token-secret")
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("operator-token-secret"));
        server.abort();
    }
}

fn tokens(answer: Value) -> Result<DeviceTokens, ApiError> {
    serde_json::from_value(answer).map_err(|_| malformed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_bounded() {
        assert_eq!(retry_delay(Some("42")), Duration::from_secs(42));
        assert_eq!(retry_delay(Some("0")), Duration::from_secs(1));
        assert_eq!(retry_delay(Some("99999")), Duration::from_secs(900));
        assert_eq!(retry_delay(Some("soon")), Duration::from_secs(1));
        assert_eq!(retry_delay(None), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_pause_beyond_the_refresh_window_gives_up_at_once() {
        let pacer = Pacer::default();
        assert!(
            pacer
                .wait_within(Instant::now() + Duration::from_secs(1))
                .await
        );
        pacer.defer(Duration::from_secs(60)).await;
        let started = std::time::Instant::now();
        assert!(!pacer.wait_within(Instant::now() + REFRESH_WINDOW).await);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "it must not wait for the pause"
        );
    }
}
