//! auth-svc's public HTTP API (spec §5.2–§5.6), as the SDK calls it.
//!
//! Requests that are safe to repeat are retried up to three times on transport errors and
//! on 502, 503 and 504, after 0.5, 1 and 2 s: registering a device (the `registration_id`
//! makes it idempotent), reads, revocations and refreshing. auth-svc accepts a rotated
//! refresh token again only for 30 s, so a refresh attempt may take 8 s at most and no
//! attempt starts that could end later than 25 s after the first one. Signing up, signing
//! in and the email requests are never repeated by the SDK: they count against rate
//! limits.
//!
//! Nothing here logs or formats a token, a password or a response body: errors quote only
//! auth-svc's error code and message.

use std::time::Duration;

use push_domain::{DeviceId, UserId};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::SdkError;

/// How long one HTTP request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one refresh attempt may take.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(8);
/// Every refresh attempt ends within this long of the first, inside auth-svc's 30 s grace
/// for a rotated refresh token.
const REFRESH_WINDOW: Duration = Duration::from_secs(25);
/// Delays before the retries of a request that is safe to repeat.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// The kind of device, as auth-svc records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Ios,
    Android,
    Web,
    Desktop,
    Other,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Other => "other",
        }
    }
}

/// A user's public profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub user_id: UserId,
    pub handle: String,
    pub display_name: String,
}

/// One of the user's devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub device_id: DeviceId,
    pub name: String,
    pub platform: String,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    pub last_seen_at: Option<String>,
    /// The device this client runs on.
    pub current: bool,
}

/// What auth-svc answers when it signs a device in.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Tokens {
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub access_token: String,
    pub expires_in_s: i64,
    pub refresh_token: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tokens")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("expires_in_s", &self.expires_in_s)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokensBody {
    user_id: Uuid,
    device_id: Uuid,
    access_token: String,
    expires_in_s: i64,
    refresh_token: String,
}

impl From<TokensBody> for Tokens {
    fn from(body: TokensBody) -> Self {
        Self {
            user_id: UserId(body.user_id),
            device_id: DeviceId(body.device_id),
            access_token: body.access_token,
            expires_in_s: body.expires_in_s,
            refresh_token: body.refresh_token,
        }
    }
}

#[derive(Deserialize)]
struct ProfileBody {
    user_id: Uuid,
    handle: String,
    display_name: String,
}

impl From<ProfileBody> for Profile {
    fn from(body: ProfileBody) -> Self {
        Self {
            user_id: UserId(body.user_id),
            handle: body.handle,
            display_name: body.display_name,
        }
    }
}

/// auth-svc, reached at `base_url` (`https://push.example`), with `host` as the `Host`
/// header when the URL names a load balancer rather than the virtual host it routes.
#[derive(Clone)]
pub(crate) struct AuthApi {
    http: reqwest::Client,
    base_url: String,
    host: Option<String>,
    refresh_attempts: Attempts,
}

/// How long each attempt of a request may take, and the time from the first attempt by
/// which every attempt must have ended.
#[derive(Debug, Clone, Copy)]
struct Attempts {
    timeout: Duration,
    window: Option<Duration>,
}

impl Attempts {
    const ORDINARY: Self = Self {
        timeout: REQUEST_TIMEOUT,
        window: None,
    };
    const REFRESH: Self = Self {
        timeout: REFRESH_TIMEOUT,
        window: Some(REFRESH_WINDOW),
    };
}

impl AuthApi {
    pub fn new(base_url: &str, host: Option<&str>) -> Result<Self, SdkError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .build()
            .map_err(|error| SdkError::Network(error.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            host: host.map(str::to_owned),
            refresh_attempts: Attempts::REFRESH,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let request = self
            .http
            .request(method, format!("{}{path}", self.base_url));
        match &self.host {
            Some(host) => request.header(reqwest::header::HOST, host),
            None => request,
        }
    }

    /// `POST /v1/auth/signup`: answered the same whether or not the email is known.
    pub async fn sign_up(
        &self,
        email: &str,
        password: &str,
        handle: &str,
        display_name: &str,
    ) -> Result<(), SdkError> {
        let body = json!({
            "email": email,
            "password": password,
            "handle": handle,
            "display_name": display_name,
        });
        once(self.request(Method::POST, "/v1/auth/signup").json(&body)).await?;
        Ok(())
    }

    /// `POST /v1/auth/verify/resend`.
    pub async fn resend_verification(&self, email: &str) -> Result<(), SdkError> {
        let body = json!({ "email": email });
        once(
            self.request(Method::POST, "/v1/auth/verify/resend")
                .json(&body),
        )
        .await?;
        Ok(())
    }

    /// `POST /v1/auth/password/forgot`.
    pub async fn forgot_password(&self, email: &str) -> Result<(), SdkError> {
        let body = json!({ "email": email });
        once(
            self.request(Method::POST, "/v1/auth/password/forgot")
                .json(&body),
        )
        .await?;
        Ok(())
    }

    /// `POST /v1/auth/login`: a registration token, good for registering one device.
    pub async fn login(&self, email: &str, password: &str) -> Result<String, SdkError> {
        let body = json!({ "email": email, "password": password });
        let answer = once(self.request(Method::POST, "/v1/auth/login").json(&body)).await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| unreadable("the sign-in answer"))
    }

    /// `POST /v1/auth/login` with an active device: its new tokens.
    pub async fn login_device(
        &self,
        email: &str,
        password: &str,
        device: DeviceId,
    ) -> Result<Tokens, SdkError> {
        let body = json!({ "email": email, "password": password, "device_id": device });
        let answer = once(self.request(Method::POST, "/v1/auth/login").json(&body)).await?;
        tokens(answer)
    }

    /// `POST /v1/devices`: registers a device, or finds the one an earlier attempt with
    /// the same `registration_id` registered.
    pub async fn register_device(
        &self,
        registration_token: &str,
        registration_id: Uuid,
        name: &str,
        platform: Platform,
    ) -> Result<Tokens, SdkError> {
        let body = json!({
            "name": name,
            "platform": platform.as_str(),
            "registration_id": registration_id,
        });
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::POST, "/v1/devices")
                .bearer_auth(registration_token)
                .json(&body)
        })
        .await?;
        tokens(answer)
    }

    /// `POST /v1/auth/refresh`: new tokens; the refresh token is rotated.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, SdkError> {
        let body = json!({ "refresh_token": refresh_token });
        let answer = retried(self.refresh_attempts, || {
            self.request(Method::POST, "/v1/auth/refresh").json(&body)
        })
        .await?;
        tokens(answer)
    }

    /// `GET /v1/devices`.
    pub async fn devices(&self, access_token: &str) -> Result<Vec<Device>, SdkError> {
        #[derive(Deserialize)]
        struct DeviceBody {
            device_id: Uuid,
            name: String,
            platform: String,
            created_at: String,
            last_seen_at: Option<String>,
            current: bool,
        }
        #[derive(Deserialize)]
        struct DevicesBody {
            devices: Vec<DeviceBody>,
        }
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/devices")
                .bearer_auth(access_token)
        })
        .await?;
        let body: DevicesBody =
            serde_json::from_value(answer).map_err(|_| unreadable("the device list"))?;
        Ok(body
            .devices
            .into_iter()
            .map(|device| Device {
                device_id: DeviceId(device.device_id),
                name: device.name,
                platform: device.platform,
                created_at: device.created_at,
                last_seen_at: device.last_seen_at,
                current: device.current,
            })
            .collect())
    }

    /// `DELETE /v1/devices/{id}`.
    pub async fn revoke_device(
        &self,
        access_token: &str,
        device: DeviceId,
    ) -> Result<(), SdkError> {
        let path = format!("/v1/devices/{device}");
        retried(Attempts::ORDINARY, || {
            self.request(Method::DELETE, &path)
                .bearer_auth(access_token)
        })
        .await?;
        Ok(())
    }

    /// `GET /v1/users?ids=…`: the profiles among `ids` the caller may see.
    pub async fn profiles(
        &self,
        access_token: &str,
        ids: &[UserId],
    ) -> Result<Vec<Profile>, SdkError> {
        #[derive(Deserialize)]
        struct UsersBody {
            users: Vec<ProfileBody>,
        }
        let ids: Vec<String> = ids.iter().map(UserId::to_string).collect();
        let ids = ids.join(",");
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/users")
                .query(&[("ids", ids.as_str())])
                .bearer_auth(access_token)
        })
        .await?;
        let body: UsersBody =
            serde_json::from_value(answer).map_err(|_| unreadable("the profiles"))?;
        Ok(body.users.into_iter().map(Profile::from).collect())
    }

    /// `GET /v1/users/lookup?handle=…`.
    pub async fn lookup(&self, access_token: &str, handle: &str) -> Result<Profile, SdkError> {
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/users/lookup")
                .query(&[("handle", handle)])
                .bearer_auth(access_token)
        })
        .await?;
        serde_json::from_value::<ProfileBody>(answer)
            .map(Profile::from)
            .map_err(|_| unreadable("the profile"))
    }
}

fn tokens(answer: Value) -> Result<Tokens, SdkError> {
    serde_json::from_value::<TokensBody>(answer)
        .map(Tokens::from)
        .map_err(|_| unreadable("the tokens"))
}

fn unreadable(what: &str) -> SdkError {
    SdkError::Network(format!("auth-svc answered {what} in an unexpected form"))
}

/// Sends a request once; its JSON answer (`null` for an empty one).
async fn once(request: RequestBuilder) -> Result<Value, SdkError> {
    let response = request
        .send()
        .await
        .map_err(|error| SdkError::Network(error.without_url().to_string()))?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    let body = response
        .bytes()
        .await
        .map_err(|error| SdkError::Network(error.without_url().to_string()))?;
    if status.is_success() {
        return Ok(serde_json::from_slice(&body).unwrap_or(Value::Null));
    }
    Err(refusal(status, &body, retry_after))
}

/// Sends a request that is safe to repeat, retrying transport errors and 502, 503 and
/// 504 a few times, each attempt within `attempts.timeout`; with a window, no attempt
/// starts that could end after it.
async fn retried(
    attempts: Attempts,
    request: impl Fn() -> RequestBuilder,
) -> Result<Value, SdkError> {
    let started = tokio::time::Instant::now();
    let mut delays = RETRY_DELAYS.iter();
    loop {
        match once(request().timeout(attempts.timeout)).await {
            Err(error) if is_transient(&error) => match delays.next() {
                Some(delay)
                    if attempts.window.is_none_or(|window| {
                        started.elapsed() + *delay + attempts.timeout <= window
                    }) =>
                {
                    tokio::time::sleep(*delay).await;
                }
                _ => return Err(error),
            },
            answer => return answer,
        }
    }
}

fn is_transient(error: &SdkError) -> bool {
    match error {
        SdkError::Network(_) => true,
        SdkError::Server { code, .. } => {
            matches!(
                code.as_str(),
                "unavailable" | "http_502" | "http_503" | "http_504"
            )
        }
        _ => false,
    }
}

/// An error answer: auth-svc's `{"error": {"code", "message", "retryable"}}`, or the
/// status alone when the body is something else (a proxy's page). The body is never
/// quoted otherwise.
fn refusal(status: StatusCode, body: &[u8], retry_after_s: Option<u32>) -> SdkError {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: ErrorDetail,
    }
    #[derive(Deserialize)]
    struct ErrorDetail {
        code: String,
        message: String,
        #[serde(default)]
        retryable: bool,
    }
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(ErrorBody { error }) => SdkError::Server {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
            retry_after_s,
        },
        Err(_) => SdkError::Server {
            code: format!("http_{}", status.as_u16()),
            message: format!("auth-svc answered {status}"),
            retryable: matches!(status.as_u16(), 502..=504),
            retry_after_s,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode as Status};
    use axum::response::IntoResponse;
    use axum::routing::post;

    use super::*;

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    fn tokens_json() -> Value {
        json!({
            "user_id": Uuid::now_v7(),
            "device_id": Uuid::now_v7(),
            "token_type": "Bearer",
            "access_token": "access-1",
            "expires_in_s": 900,
            "refresh_token": "refresh-1",
            "refresh_expires_in_s": 2_592_000,
        })
    }

    #[tokio::test]
    async fn a_refresh_is_retried_while_the_service_is_unavailable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(move || {
                let counter = counter.clone();
                async move {
                    if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                        let error = json!({"error": {"code": "unavailable", "message": "try again", "retryable": true}});
                        (Status::SERVICE_UNAVAILABLE, axum::Json(error)).into_response()
                    } else {
                        axum::Json(tokens_json()).into_response()
                    }
                }
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let tokens = api.refresh("refresh-0").await.unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(!format!("{tokens:?}").contains("access-1"));
    }

    /// A refresh retry must not reach auth-svc after its grace for the rotated token: each
    /// attempt is cut off, and none starts that could end after the window.
    #[tokio::test]
    async fn refresh_attempts_end_within_the_window() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    axum::Json(tokens_json()).into_response()
                }
            }),
        );
        let mut api = AuthApi::new(&serve(router).await, None).unwrap();
        // The real 8 s and 25 s, scaled down: two attempts fit, a third would end late.
        api.refresh_attempts = Attempts {
            timeout: Duration::from_millis(300),
            window: Some(Duration::from_millis(1_400)),
        };
        let started = std::time::Instant::now();
        let error = api.refresh("refresh-0").await.unwrap_err();
        assert_eq!(error.code(), "network");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() < Duration::from_millis(1_400));
    }

    #[tokio::test]
    async fn a_sign_in_is_never_repeated_and_errors_keep_the_code() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = Router::new().route(
            "/v1/auth/login",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let error = json!({"error": {"code": "unavailable", "message": "busy", "retryable": true}});
                    (Status::SERVICE_UNAVAILABLE, [("retry-after", "1")], axum::Json(error))
                }
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let error = api.login("a@example.com", "password").await.unwrap_err();
        assert_eq!(
            error,
            SdkError::Server {
                code: "unavailable".to_owned(),
                message: "busy".to_owned(),
                retryable: true,
                retry_after_s: Some(1),
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_body_that_is_not_an_api_error_is_never_quoted() {
        let router = Router::new().route(
            "/v1/auth/refresh",
            post(|| async { (Status::UNAUTHORIZED, "secret-looking body") }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let error = api.refresh("refresh-0").await.unwrap_err();
        assert_eq!(error.code(), "http_401");
        assert!(!error.to_string().contains("secret-looking"));
    }

    #[tokio::test]
    async fn the_host_override_and_the_bearer_token_are_sent() {
        let router = Router::new().route(
            "/v1/devices",
            post(|headers: HeaderMap| async move {
                let host = headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let bearer = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if host == "push.local" && bearer == "Bearer registration-1" {
                    (Status::CREATED, axum::Json(tokens_json())).into_response()
                } else {
                    Status::BAD_REQUEST.into_response()
                }
            }),
        );
        let api = AuthApi::new(&serve(router).await, Some("push.local")).unwrap();
        let tokens = api
            .register_device("registration-1", Uuid::now_v7(), "phone", Platform::Ios)
            .await
            .unwrap();
        assert_eq!(tokens.refresh_token, "refresh-1");
    }
}
