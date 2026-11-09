//! auth-svc's public HTTP API (spec §5.2–§5.6), as the SDK calls it.
//!
//! Requests that are safe to repeat are retried up to three times on transport errors and
//! on 502, 503 and 504, after 0.5, 1 and 2 s: registering a device (the `registration_id`
//! makes it idempotent), reads, revocations and refreshing; of the calls API (docs/calls.md
//! §12, §13.1), `GET /v1/calls/ice-servers` and `GET /v1/calls` (reads) and
//! `DELETE /v1/calls/{id}` (hiding is idempotent). auth-svc accepts a rotated
//! refresh token again only for 30 s, so a refresh attempt may take 8 s at most and no
//! attempt starts that could end later than 25 s after the first one. Signing up, signing
//! in and the email requests are never repeated by the SDK: they count against rate
//! limits.
//!
//! Nothing here logs or formats a token, a password or a response body: errors quote only
//! auth-svc's error code and message.

use std::time::Duration;

use push_domain::{CallId, DeviceId, UserId};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::SdkError;
use crate::calls::{CallDirection, CallEndReason, CallLogEntry, CallPage, IceServer, IceServers};

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

/// A user's public profile. A deleted account the user could see before (a shared
/// conversation or contact) is a tombstone: `deleted` is set, `display_name` is "Deleted
/// agent" or "Deleted account", `handle` is empty, `owner_user_id` is `None` and `kind`
/// is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub user_id: UserId,
    /// Empty for a deleted account.
    pub handle: String,
    pub display_name: String,
    /// `user` or `agent`.
    pub kind: String,
    pub owner_user_id: Option<UserId>,
    /// An agent's owner, as the owner's account says now: show "Chats with <agent> are
    /// visible to its owner, <owner>" (spec 2026-11-06 agent connections §7). `None` for
    /// people, tombstones and servers before agent connections.
    pub owner_display_name: Option<String>,
    pub owner_handle: Option<String>,
    /// The account was deleted: this is its tombstone.
    pub deleted: bool,
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
pub(crate) struct ProfileBody {
    user_id: Uuid,
    handle: String,
    display_name: String,
    #[serde(default = "user_kind")]
    kind: String,
    owner_user_id: Option<Uuid>,
    /// Absent from servers before agent connections.
    #[serde(default)]
    owner_display_name: Option<String>,
    #[serde(default)]
    owner_handle: Option<String>,
    /// Absent from servers before tombstones.
    #[serde(default)]
    deleted: bool,
}

fn user_kind() -> String {
    "user".to_owned()
}

impl From<ProfileBody> for Profile {
    fn from(body: ProfileBody) -> Self {
        Self {
            user_id: UserId(body.user_id),
            handle: body.handle,
            display_name: body.display_name,
            kind: body.kind,
            owner_user_id: body.owner_user_id.map(UserId),
            owner_display_name: body.owner_display_name,
            owner_handle: body.owner_handle,
            deleted: body.deleted,
        }
    }
}

#[cfg(test)]
mod profile_contract {
    use super::*;

    #[test]
    fn agent_identity_and_nullable_owner_survive_profile_conversion() {
        let (user, owner) = (Uuid::new_v4(), Uuid::new_v4());
        let body: ProfileBody = serde_json::from_value(json!({
            "user_id":user, "handle":"my_agent", "display_name":"Agent",
            "kind":"agent", "owner_user_id":owner,
        }))
        .unwrap();
        let profile = Profile::from(body);
        assert_eq!(profile.kind, "agent");
        assert_eq!(profile.owner_user_id, Some(UserId(owner)));
        assert!(!profile.deleted);
    }

    #[test]
    fn an_agent_profile_names_its_owner_for_the_notice() {
        let (user, owner) = (Uuid::new_v4(), Uuid::new_v4());
        let body: ProfileBody = serde_json::from_value(json!({
            "user_id": user, "handle": "coda", "display_name": "Coda", "kind": "agent",
            "owner_user_id": owner, "owner_display_name": "Yuri", "owner_handle": "yuri",
            "deleted": false, "state": "active",
        }))
        .unwrap();
        let profile = Profile::from(body);
        assert_eq!(profile.owner_display_name.as_deref(), Some("Yuri"));
        assert_eq!(profile.owner_handle.as_deref(), Some("yuri"));
    }

    #[test]
    fn a_tombstone_parses_with_an_empty_handle_and_no_owner() {
        let user = Uuid::new_v4();
        let body: ProfileBody = serde_json::from_value(json!({
            "user_id": user, "handle": "", "display_name": "Deleted agent",
            "kind": "agent", "owner_user_id": null, "deleted": true, "state": "deleted",
        }))
        .unwrap();
        let profile = Profile::from(body);
        assert!(profile.deleted);
        assert_eq!(
            (
                profile.handle.as_str(),
                profile.display_name.as_str(),
                profile.kind.as_str()
            ),
            ("", "Deleted agent", "agent")
        );
        assert_eq!(profile.owner_user_id, None);
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
pub(crate) struct Attempts {
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

    pub(crate) fn request(&self, method: Method, path: &str) -> RequestBuilder {
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

    /// `POST /v1/auth/entra`: a registration token for the account of an Entra ID access
    /// token for auth-svc's API; `handle` names a new account only. Never repeated: it
    /// counts against rate limits.
    pub async fn entra(
        &self,
        access_token: &str,
        handle: Option<&str>,
    ) -> Result<String, SdkError> {
        let body = json!({ "token": access_token, "handle": handle });
        let answer = once(self.request(Method::POST, "/v1/auth/entra").json(&body)).await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| unreadable("the sign-in answer"))
    }

    /// `POST /v1/auth/entra` with an active device: its new tokens and a new session.
    pub async fn entra_device(
        &self,
        access_token: &str,
        handle: Option<&str>,
        device: DeviceId,
    ) -> Result<Tokens, SdkError> {
        let body = json!({ "token": access_token, "handle": handle, "device_id": device });
        let answer = once(self.request(Method::POST, "/v1/auth/entra").json(&body)).await?;
        tokens(answer)
    }

    /// `POST /v1/auth/agent-key`: a registration token for the agent whose key signed
    /// `assertion`; with `enrollment_code`, the owner's one-time code, it enrolls the
    /// agent first. Never repeated: each assertion is single-use.
    pub async fn agent_key(
        &self,
        assertion: &str,
        enrollment_code: Option<&str>,
    ) -> Result<String, SdkError> {
        let body = match enrollment_code {
            Some(code) => json!({ "token": assertion, "enrollment_code": code }),
            None => json!({ "token": assertion }),
        };
        let answer = once(self.request(Method::POST, "/v1/auth/agent-key").json(&body)).await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| unreadable("the sign-in answer"))
    }

    /// `POST /v1/auth/agent-key` with an active device: its new tokens and a new session.
    /// Never repeated: each assertion is single-use.
    pub async fn agent_key_device(
        &self,
        assertion: &str,
        device: DeviceId,
    ) -> Result<Tokens, SdkError> {
        let body = json!({ "token": assertion, "device_id": device });
        let answer = once(self.request(Method::POST, "/v1/auth/agent-key").json(&body)).await?;
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

    /// `PATCH /v1/users/me {display_name?, handle?}`: the caller's profile after the
    /// change. Never repeated by the SDK: a handle change counts against rate limits.
    pub async fn update_profile(
        &self,
        access_token: &str,
        display_name: Option<&str>,
        handle: Option<&str>,
    ) -> Result<Profile, SdkError> {
        let mut body = serde_json::Map::new();
        if let Some(display_name) = display_name {
            body.insert("display_name".to_owned(), json!(display_name));
        }
        if let Some(handle) = handle {
            body.insert("handle".to_owned(), json!(handle));
        }
        let answer = once(
            self.request(Method::PATCH, "/v1/users/me")
                .bearer_auth(access_token)
                .json(&body),
        )
        .await?;
        serde_json::from_value::<ProfileBody>(answer)
            .map(Profile::from)
            .map_err(|_| unreadable("the profile"))
    }

    /// `GET /v1/calls/ice-servers`: the STUN and TURN servers for the user's calls.
    pub async fn ice_servers(&self, access_token: &str) -> Result<IceServers, SdkError> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Urls {
            // The WebRTC dictionary allows a string.
            One(String),
            Many(Vec<String>),
        }
        #[derive(Deserialize)]
        struct ServerBody {
            urls: Urls,
            username: Option<String>,
            credential: Option<String>,
        }
        #[derive(Deserialize)]
        struct ServersBody {
            ice_servers: Vec<ServerBody>,
            expires_at: String,
            relay: bool,
        }
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/calls/ice-servers")
                .bearer_auth(access_token)
        })
        .await?;
        let body: ServersBody =
            serde_json::from_value(answer).map_err(|_| unreadable("the ICE servers"))?;
        Ok(IceServers {
            servers: body
                .ice_servers
                .into_iter()
                .map(|server| IceServer {
                    urls: match server.urls {
                        Urls::One(url) => vec![url],
                        Urls::Many(urls) => urls,
                    },
                    username: server.username,
                    credential: server.credential,
                })
                .collect(),
            expires_at_ms: millis(&body.expires_at).ok_or_else(|| unreadable("the ICE servers"))?,
            relay: body.relay,
        })
    }

    /// `GET /v1/calls?before=…&since=…&limit=…`: a page of the user's call history; absent
    /// parameters are not sent.
    pub async fn call_history(
        &self,
        access_token: &str,
        before: Option<&str>,
        since: Option<&str>,
        limit: Option<u32>,
    ) -> Result<CallPage, SdkError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(before) = before {
            query.push(("before", before.to_owned()));
        }
        if let Some(since) = since {
            query.push(("since", since.to_owned()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let answer = retried(Attempts::ORDINARY, || {
            self.request(Method::GET, "/v1/calls")
                .query(&query)
                .bearer_auth(access_token)
        })
        .await?;
        call_page(answer)
    }

    /// `DELETE /v1/calls/{call_id}`: hides the call from the user's history; `204`
    /// whether or not they had such a call.
    pub async fn hide_call(&self, access_token: &str, call: CallId) -> Result<(), SdkError> {
        let path = format!("/v1/calls/{call}");
        retried(Attempts::ORDINARY, || {
            self.request(Method::DELETE, &path)
                .bearer_auth(access_token)
        })
        .await?;
        Ok(())
    }
}

/// A page of call history as `GET /v1/calls` answers it.
pub(crate) fn call_page(answer: Value) -> Result<CallPage, SdkError> {
    #[derive(Deserialize)]
    struct EntryBody {
        call_id: CallId,
        peer_user_id: UserId,
        direction: String,
        started_at: String,
        answered_at: Option<String>,
        ended_at: String,
        duration_s: u32,
        outcome: String,
        #[serde(default)]
        started_by_agent: bool,
        cursor: String,
    }
    #[derive(Deserialize)]
    struct PageBody {
        calls: Vec<EntryBody>,
        next: Option<String>,
    }
    let body: PageBody =
        serde_json::from_value(answer).map_err(|_| unreadable("the call history"))?;
    let entry = |entry: EntryBody| -> Option<CallLogEntry> {
        Some(CallLogEntry {
            call_id: entry.call_id,
            peer_user_id: entry.peer_user_id,
            direction: match entry.direction.as_str() {
                "in" => CallDirection::Incoming,
                "out" => CallDirection::Outgoing,
                // The direction set is closed, so an unknown one is a broken answer
                // and fails the page; outcomes may grow, so an unknown one is Failed.
                _ => return None,
            },
            started_at_ms: millis(&entry.started_at)?,
            answered_at_ms: match entry.answered_at {
                Some(time) => Some(millis(&time)?),
                None => None,
            },
            ended_at_ms: millis(&entry.ended_at)?,
            duration_s: entry.duration_s,
            outcome: CallEndReason::from_name(&entry.outcome),
            started_by_agent: entry.started_by_agent,
            cursor: entry.cursor,
        })
    };
    let calls = body
        .calls
        .into_iter()
        .map(entry)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| unreadable("the call history"))?;
    Ok(CallPage {
        calls,
        next: body.next,
    })
}

/// The attempts of an ordinary request.
pub(crate) const ORDINARY: Attempts = Attempts::ORDINARY;

/// An RFC 3339 time as milliseconds since the Unix epoch.
pub(crate) fn millis(time: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn tokens(answer: Value) -> Result<Tokens, SdkError> {
    serde_json::from_value::<TokensBody>(answer)
        .map(Tokens::from)
        .map_err(|_| unreadable("the tokens"))
}

pub(crate) fn unreadable(what: &str) -> SdkError {
    SdkError::Network(format!("auth-svc answered {what} in an unexpected form"))
}

/// Sends a request once; its JSON answer (`null` for an empty one).
pub(crate) async fn once(request: RequestBuilder) -> Result<Value, SdkError> {
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
pub(crate) async fn retried(
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
            action_id: None,
        },
        Err(_) => SdkError::Server {
            code: format!("http_{}", status.as_u16()),
            message: format!("auth-svc answered {status}"),
            retryable: matches!(status.as_u16(), 502..=504),
            retry_after_s,
            action_id: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::{Path, Query};
    use axum::http::{HeaderMap, StatusCode as Status};
    use axum::response::IntoResponse;
    use axum::routing::{delete, get, post};

    use super::*;
    use crate::calls::{CallDirection, CallEndReason, CallLogEntry, IceServer};

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
                action_id: None,
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

    fn bearer(headers: &HeaderMap) -> &str {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    #[tokio::test]
    async fn ice_servers_are_read_as_the_guide_shows_them() {
        let router = Router::new().route(
            "/v1/calls/ice-servers",
            get(|headers: HeaderMap| async move {
                if bearer(&headers) != "Bearer access-1" {
                    return Status::UNAUTHORIZED.into_response();
                }
                axum::Json(json!({
                    "ice_servers": [
                        { "urls": ["stun:turn.example.com:3478"] },
                        {
                            "urls": [
                                "turn:turn.example.com:3478?transport=udp",
                                "turns:turn.example.com:5349?transport=tcp"
                            ],
                            "username": "1793729000:0193f0c2-6f0e-7c4d-9a2b-1c3d4e5f6a7b",
                            "credential": "VQi7KgKAXiAc6ZhbTP2coGjJMjw="
                        },
                        { "urls": "stun:stun.example.org" }
                    ],
                    "expires_at": "2026-11-03T18:43:20Z",
                    "relay": true
                }))
                .into_response()
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let servers = api.ice_servers("access-1").await.unwrap();
        assert_eq!(
            servers,
            IceServers {
                servers: vec![
                    IceServer {
                        urls: vec!["stun:turn.example.com:3478".to_owned()],
                        username: None,
                        credential: None,
                    },
                    IceServer {
                        urls: vec![
                            "turn:turn.example.com:3478?transport=udp".to_owned(),
                            "turns:turn.example.com:5349?transport=tcp".to_owned(),
                        ],
                        username: Some(
                            "1793729000:0193f0c2-6f0e-7c4d-9a2b-1c3d4e5f6a7b".to_owned()
                        ),
                        credential: Some("VQi7KgKAXiAc6ZhbTP2coGjJMjw=".to_owned()),
                    },
                    IceServer {
                        urls: vec!["stun:stun.example.org".to_owned()],
                        username: None,
                        credential: None,
                    },
                ],
                expires_at_ms: 1_793_731_400_000,
                relay: true,
            }
        );
        let debug = format!("{servers:?}");
        assert!(!debug.contains("VQi7KgKAXiAc6ZhbTP2coGjJMjw="));
        assert!(!debug.contains("1793729000:"));
        assert!(debug.contains("turn.example.com"));
    }

    #[tokio::test]
    async fn turn_unavailable_is_returned_at_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let router = Router::new().route(
            "/v1/calls/ice-servers",
            get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let error = json!({"error": {"code": "turn_unavailable", "message": "no relay", "retryable": true}});
                    (Status::SERVICE_UNAVAILABLE, [("retry-after", "30")], axum::Json(error))
                }
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let error = api.ice_servers("access-1").await.unwrap_err();
        assert_eq!(
            error,
            SdkError::Server {
                code: "turn_unavailable".to_owned(),
                message: "no relay".to_owned(),
                retryable: true,
                retry_after_s: Some(30),
                action_id: None,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unreadable_ice_answer_is_a_network_error() {
        let router = Router::new().route(
            "/v1/calls/ice-servers",
            get(|| async {
                axum::Json(json!({"ice_servers": [], "expires_at": "soon", "relay": false}))
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let error = api.ice_servers("access-1").await.unwrap_err();
        assert_eq!(error.code(), "network");
    }

    #[tokio::test]
    async fn the_call_history_sends_its_query_and_reads_the_page() {
        let (call, peer) = (CallId::new(), UserId::new());
        let router = Router::new().route(
            "/v1/calls",
            get(
                move |headers: HeaderMap, Query(query): Query<HashMap<String, String>>| async move {
                    let expected: HashMap<String, String> = [
                        ("before", "b-cursor"),
                        ("since", "s-cursor"),
                        ("limit", "2"),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect();
                    if bearer(&headers) != "Bearer access-1" || query != expected {
                        return Status::BAD_REQUEST.into_response();
                    }
                    axum::Json(json!({
                        "calls": [
                            {
                                "call_id": call,
                                "peer_user_id": peer,
                                "direction": "in",
                                "started_at": "2026-11-03T17:41:05.120Z",
                                "answered_at": "2026-11-03T17:41:09.002Z",
                                "ended_at": "2026-11-03T17:43:10.877Z",
                                "duration_s": 121,
                                "outcome": "completed",
                                "started_by_agent": true,
                                "cursor": "c1"
                            },
                            {
                                "call_id": call,
                                "peer_user_id": peer,
                                "direction": "out",
                                "started_at": "2026-11-03T17:40:00Z",
                                "ended_at": "2026-11-03T17:40:30Z",
                                "duration_s": 0,
                                "outcome": "something-new",
                                "cursor": "c2"
                            }
                        ],
                        "next": "c2"
                    }))
                    .into_response()
                },
            ),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let page = api
            .call_history("access-1", Some("b-cursor"), Some("s-cursor"), Some(2))
            .await
            .unwrap();
        assert_eq!(page.next.as_deref(), Some("c2"));
        assert_eq!(
            page.calls[0],
            CallLogEntry {
                call_id: call,
                peer_user_id: peer,
                direction: CallDirection::Incoming,
                started_at_ms: 1_793_727_665_120,
                answered_at_ms: Some(1_793_727_669_002),
                ended_at_ms: 1_793_727_790_877,
                duration_s: 121,
                outcome: CallEndReason::Completed,
                started_by_agent: true,
                cursor: "c1".to_owned(),
            }
        );
        assert_eq!(page.calls[1].direction, CallDirection::Outgoing);
        assert_eq!(page.calls[1].answered_at_ms, None);
        assert_eq!(page.calls[1].outcome, CallEndReason::Failed);
        assert!(!page.calls[1].started_by_agent);
    }

    #[tokio::test]
    async fn the_call_history_omits_absent_parameters_and_rejects_garbled_entries() {
        let router = Router::new().route(
            "/v1/calls",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                if !query.is_empty() {
                    return Status::BAD_REQUEST.into_response();
                }
                axum::Json(json!({
                    "calls": [{
                        "call_id": Uuid::now_v7(),
                        "peer_user_id": Uuid::now_v7(),
                        "direction": "sideways",
                        "started_at": "2026-11-03T17:40:00Z",
                        "ended_at": "2026-11-03T17:40:30Z",
                        "duration_s": 0,
                        "outcome": "missed",
                        "cursor": "c"
                    }]
                }))
                .into_response()
            }),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        let error = api
            .call_history("access-1", None, None, None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "network");
    }

    #[tokio::test]
    async fn hiding_a_call_deletes_it_by_id() {
        let call = CallId::new();
        let router = Router::new().route(
            "/v1/calls/{call_id}",
            delete(
                move |headers: HeaderMap, Path(id): Path<String>| async move {
                    if bearer(&headers) == "Bearer access-1" && id == call.to_string() {
                        Status::NO_CONTENT
                    } else {
                        Status::BAD_REQUEST
                    }
                },
            ),
        );
        let api = AuthApi::new(&serve(router).await, None).unwrap();
        api.hide_call("access-1", call).await.unwrap();
    }
}
