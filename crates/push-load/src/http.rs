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
        let code = answer["error"]["code"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        Err(ApiError::Rejected {
            status: status.as_u16(),
            code,
        })
    }

    /// Waits out rate limits (pausing every worker) and retries up to five transport
    /// failures; every request here is safe to repeat (registration is idempotent by
    /// `registration_id`, a repeated refresh falls in the 30 s grace).
    async fn post(
        &self,
        path: &str,
        body: Value,
        bearer: Option<&str>,
        expected: StatusCode,
    ) -> Result<Value, ApiError> {
        let mut failures = 0;
        loop {
            self.pacer.wait().await;
            match self.post_once(path, &body, bearer, expected).await {
                Err(ApiError::RetryAfter(delay)) => self.pacer.defer(delay).await,
                Err(ApiError::Transport(error)) => {
                    failures += 1;
                    if failures >= TRANSPORT_ATTEMPTS {
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
            )
            .await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(malformed)
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
            self.post("/v1/auth/login", body, None, StatusCode::OK)
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
            )
            .await?,
        )
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<DeviceTokens, ApiError> {
        let body = json!({"refresh_token": refresh_token});
        tokens(
            self.post("/v1/auth/refresh", body, None, StatusCode::OK)
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
}
