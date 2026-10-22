//! The public HTTP API (spec §2, §5; Entra ID sign-in: spec 2026-10-19 §4): `/v1/auth/*`,
//! `/v1/devices/*` and `/v1/users*`, JSON in and out, plus the two small pages that email
//! links open.
//!
//! Errors are `{"error": {"code", "message", "retryable"}}` with the status of
//! [`AuthError::status`]; 429 answers carry `Retry-After`, 401 answers
//! `WWW-Authenticate: Bearer`. No answer may be cached (`Cache-Control: no-store`) except
//! the JWKS. Pages send `Referrer-Policy: no-referrer`, so the token in their URL never
//! leaks to another site (spec §5.6), and change nothing on `GET`: the change is the
//! `POST` their button sends, so a mail scanner that opens links does nothing.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    REFERRER_POLICY, RETRY_AFTER, WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::request::Parts;
use axum::http::{Extensions, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Form, Json, Router};
use chrono::SecondsFormat;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, ErrorCode, Subject};
use crate::devices::{Caller, Tokens};
use crate::directory::Directory;
use crate::error::AuthError;
use crate::identity::{IdentityState, Principal, identify};
use crate::limits::{Action, Limits};
use crate::rules::{self, Invalid};
use crate::service::{Auth, Signup};

/// Largest request body accepted.
pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024;

/// What the handlers share.
#[derive(Debug, Clone)]
pub struct Api {
    pub auth: Auth,
    pub limits: Limits,
    /// The header that carries the client's IP, set by the ingress controller
    /// (`x-real-ip`); without it, the peer address. Only trust a header the proxy in front
    /// always overwrites (or, for `X-Forwarded-For`, appends to): its last entry is used.
    pub client_ip_header: Option<HeaderName>,
    /// Who may see whose profile: message-svc's `Directory` API.
    pub directory: Arc<dyn Directory>,
}

/// The router of the public API. Without email and password sign-in its routes do not
/// exist (404); `/v1/auth/{provider}` exists for each offered authenticator only, Entra
/// ID's at `/v1/auth/entra` (spec 2026-10-19 D9; pluggable identity D7).
pub fn router(api: Api) -> Router {
    let mut routes: Router<Api> = Router::new();
    if api.auth.password_enabled() {
        routes = routes
            .route("/v1/auth/signup", post(signup))
            .route("/v1/auth/verify", get(verify_page).post(verify))
            .route("/v1/auth/verify/resend", post(resend))
            .route("/v1/auth/login", post(login))
            .route("/v1/auth/password/forgot", post(forgot))
            .route("/v1/auth/password/reset", get(reset_page).post(reset));
    }
    for authenticator in api.auth.authenticators() {
        let path = format!("/v1/auth/{}", authenticator.provider());
        let state = IdentityState {
            api: api.clone(),
            authenticator,
        };
        routes = routes.route(
            &path,
            post(sign_in).route_layer(axum::middleware::from_fn_with_state(state, identify)),
        );
    }
    routes
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/jwks", get(jwks))
        .route("/v1/devices", post(register_device).get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/agents", post(reserve_agent).get(list_agents))
        .route(
            "/v1/agents/{agent_id}",
            delete(delete_agent).patch(update_agent),
        )
        .route("/v1/users", get(profiles))
        .route("/v1/users/me", get(own_profile).patch(update_profile))
        .route("/v1/users/lookup", get(lookup))
        .route_layer(axum::middleware::from_fn_with_state(
            api.clone(),
            audit::audit,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::map_response(no_store_by_default))
        .with_state(api)
}

async fn no_store_by_default(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status()).expect("a valid status");
        let body = Json(json!({
            "error": {
                "code": self.code(),
                "message": self.to_string(),
                "retryable": self.retryable(),
            }
        }));
        let mut response = (status, body).into_response();
        response.extensions_mut().insert(ErrorCode(self.code()));
        let headers = response.headers_mut();
        match self {
            Self::RateLimited { retry_after_s } => {
                headers.insert(RETRY_AFTER, HeaderValue::from(retry_after_s));
            }
            Self::Unavailable => {
                headers.insert(RETRY_AFTER, HeaderValue::from_static("1"));
            }
            Self::EntraUnavailable | Self::IdentityUnavailable => {
                headers.insert(RETRY_AFTER, HeaderValue::from_static("5"));
            }
            Self::Unauthorized
            | Self::TokenExpired
            | Self::InvalidCredentials
            | Self::EntraTokenInvalid
            | Self::TokenInvalid
            | Self::SessionExpired => {
                headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
            _ => {}
        }
        response
    }
}

/// A JSON body; anything else is an `invalid` error in the API's format.
struct JsonBody<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = AuthError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|_| bad_body())
    }
}

pub(crate) fn bad_body() -> AuthError {
    AuthError::Invalid(Invalid {
        field: "body",
        reason: "expected a JSON object with the documented fields",
    })
}

/// A body from an API client (JSON) or from one of the pages (a form).
enum Submitted<T> {
    Json(T),
    Form(T),
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Submitted<T> {
    type Rejection = AuthError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_form = request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));
        if is_form {
            Form::<T>::from_request(request, state)
                .await
                .map(|Form(value)| Self::Form(value))
                .map_err(|_| bad_body())
        } else {
            JsonBody::<T>::from_request(request, state)
                .await
                .map(|JsonBody(value)| Self::Json(value))
        }
    }
}

/// The client's IP: from the configured header, else the peer address.
struct ClientIp(Option<IpAddr>);

impl FromRequestParts<Api> for ClientIp {
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, api: &Api) -> Result<Self, Self::Rejection> {
        Ok(Self(client_ip(api, &parts.headers, &parts.extensions)))
    }
}

/// The client's IP: from the configured header, else the peer address.
pub(crate) fn client_ip(api: &Api, headers: &HeaderMap, extensions: &Extensions) -> Option<IpAddr> {
    let from_header = api.client_ip_header.as_ref().and_then(|name| {
        // The last line: a proxy that appends its own line instead of joining it must
        // not let the client's line win.
        let value = headers.get_all(name).iter().next_back()?.to_str().ok()?;
        ip_from_header(value)
    });
    let peer = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    from_header.or(peer)
}

/// The client's IP in a header value: its last entry. `X-Real-IP` holds one address,
/// which ingress-nginx overwrites with the peer it saw. In `X-Forwarded-For` the client
/// writes the first entries itself and only the last is added by the proxy in front, so
/// only the last is trusted — which assumes exactly one proxy hop.
fn ip_from_header(value: &str) -> Option<IpAddr> {
    value.rsplit(',').next()?.trim().parse().ok()
}

fn bearer(headers: &HeaderMap) -> Result<&str, AuthError> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(AuthError::Unauthorized)
}

async fn caller(api: &Api, headers: &HeaderMap) -> Result<Caller, AuthError> {
    api.auth.authenticate(bearer(headers)?).await
}

/// Tokens as JSON, with their user and device for the security log.
fn tokens_response(status: StatusCode, tokens: &Tokens) -> Response {
    let mut response = (status, Json(tokens_json(tokens))).into_response();
    response.extensions_mut().insert(Subject {
        user: Some(tokens.user_id),
        device: Some(tokens.device_id),
    });
    response
}

fn tokens_json(tokens: &Tokens) -> Value {
    json!({
        "user_id": tokens.user_id,
        "device_id": tokens.device_id,
        "token_type": "Bearer",
        "access_token": tokens.access_token,
        "expires_in_s": tokens.access_expires_in_s,
        "refresh_token": tokens.refresh_token,
        "refresh_expires_in_s": tokens.refresh_expires_in_s,
    })
}

fn check_email() -> Response {
    (StatusCode::ACCEPTED, Json(json!({"status": "check_email"}))).into_response()
}

#[derive(Deserialize)]
struct SignupBody {
    email: String,
    password: String,
    handle: String,
    display_name: String,
}

async fn signup(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<SignupBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::Signup, ip, Some(&body.email))
        .await?;
    api.auth
        .signup(Signup {
            email: body.email,
            password: body.password,
            handle: body.handle,
            display_name: body.display_name,
        })
        .await?;
    Ok(check_email())
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

async fn verify_page(Query(query): Query<TokenQuery>) -> Response {
    page(
        StatusCode::OK,
        "Verify your email address",
        &format!(
            r#"<form method="post" action="/v1/auth/verify"><input type="hidden" name="token" value="{}"><button type="submit">Verify my email address</button></form>"#,
            escape(&query.token)
        ),
    )
}

async fn verify(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    body: Submitted<TokenBody>,
) -> Result<Response, AuthError> {
    api.limits.check(Action::OpenLink, ip, None).await?;
    match body {
        Submitted::Json(body) => {
            api.auth.verify_email(&body.token).await?;
            Ok(Json(json!({"status": "verified"})).into_response())
        }
        Submitted::Form(body) => Ok(match api.auth.verify_email(&body.token).await {
            Ok(()) => page(
                StatusCode::OK,
                "Email address verified",
                "<p>Your email address is verified. You can sign in now.</p>",
            ),
            Err(error) => error_page(&error),
        }),
    }
}

#[derive(Deserialize)]
struct EmailBody {
    email: String,
}

async fn resend(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<EmailBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::ResendVerification, ip, Some(&body.email))
        .await?;
    api.auth.resend_verification(&body.email).await?;
    Ok(check_email())
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
    #[serde(default)]
    device_id: Option<Uuid>,
}

async fn login(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<LoginBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::Login, ip, Some(&body.email))
        .await?;
    Ok(match body.device_id {
        None => {
            let registration = api.auth.login(&body.email, &body.password).await?;
            Json(json!({
                "registration_token": registration.token,
                "expires_in_s": registration.expires_in_s,
            }))
            .into_response()
        }
        Some(device_id) => {
            let tokens = api
                .auth
                .login_device(&body.email, &body.password, device_id)
                .await?;
            tokens_response(StatusCode::OK, &tokens)
        }
    })
}

#[derive(Deserialize)]
struct SignInBody {
    /// The handle of a new account; ignored once the account exists.
    #[serde(default)]
    handle: Option<String>,
    #[serde(default)]
    device_id: Option<Uuid>,
    #[serde(default)]
    enrollment_code: Option<String>,
}

/// `POST /v1/auth/{provider} {token?, handle?, device_id?, enrollment_code?}` (spec 2026-10-19 Entra ID
/// sign-in D10, pluggable identity D5), behind the identity middleware, which put the
/// principal here: without `device_id` a registration token, as `POST /v1/auth/login`
/// answers; with it, new tokens for that active device and a new session.
async fn sign_in(
    State(api): State<Api>,
    Extension(principal): Extension<Principal>,
    JsonBody(body): JsonBody<SignInBody>,
) -> Result<Response, AuthError> {
    let handle = body.handle.as_deref();
    if body.enrollment_code.is_some() && body.device_id.is_some() {
        return Err(bad_body());
    }
    Ok(match body.device_id {
        None => {
            let signed_in = match body.enrollment_code.as_deref() {
                Some(code) => api.auth.enroll_agent(&principal, code).await?,
                None => api.auth.sign_in_external(&principal, handle).await?,
            };
            let mut response = Json(json!({
                "user_id": signed_in.user_id,
                "registration_token": signed_in.registration.token,
                "expires_in_s": signed_in.registration.expires_in_s,
            }))
            .into_response();
            response.extensions_mut().insert(Subject {
                user: Some(signed_in.user_id),
                device: None,
            });
            response
        }
        Some(device_id) => {
            let tokens = api
                .auth
                .sign_in_external_device(&principal, handle, device_id)
                .await?;
            tokens_response(StatusCode::OK, &tokens)
        }
    })
}

#[derive(Deserialize)]
struct AgentBody {
    handle: String,
    display_name: String,
}

async fn agent_caller(
    api: &Api,
    headers: &HeaderMap,
    ip: Option<IpAddr>,
) -> Result<Caller, AuthError> {
    let caller = caller(api, headers).await?;
    api.limits
        .check(
            Action::AgentManagement,
            ip,
            Some(&caller.user_id.to_string()),
        )
        .await?;
    Ok(caller)
}

async fn reserve_agent(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<AgentBody>,
) -> Result<Response, AuthError> {
    let caller = agent_caller(&api, &headers, ip).await?;
    let reservation = api
        .auth
        .reserve_agent(caller, &body.handle, &body.display_name)
        .await?;
    let mut response = (StatusCode::CREATED, Json(json!({
                "user_id": reservation.user_id, "enrollment_code": reservation.enrollment_code,
                "expires_in_s": crate::agents::ENROLLMENT_SECONDS, "expires_at": reservation.expires_at.to_rfc3339(),
            }))).into_response();
    response.extensions_mut().insert(Subject {
        user: Some(caller.user_id),
        device: Some(caller.device_id),
    });
    Ok(response)
}

async fn list_agents(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
) -> Result<Response, AuthError> {
    let caller = agent_caller(&api, &headers, ip).await?;
    Ok(Json(json!({"agents": api.auth.owned_agents(caller).await?})).into_response())
}

async fn update_agent(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ProfileBody>,
) -> Result<Response, AuthError> {
    let caller = agent_caller(&api, &headers, ip).await?;
    let id = id.parse().map_err(|_| AuthError::AgentNotFound)?;
    api.auth
        .change_agent(
            caller,
            id,
            body.handle.as_deref(),
            body.display_name.as_deref(),
            false,
        )
        .await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.extensions_mut().insert(Subject {
        user: Some(caller.user_id),
        device: Some(caller.device_id),
    });
    Ok(response)
}

async fn delete_agent(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(id): Path<String>,
) -> Result<Response, AuthError> {
    let caller = agent_caller(&api, &headers, ip).await?;
    let id = id.parse().map_err(|_| AuthError::AgentNotFound)?;
    api.auth.change_agent(caller, id, None, None, true).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.extensions_mut().insert(Subject {
        user: Some(caller.user_id),
        device: Some(caller.device_id),
    });
    Ok(response)
}
#[derive(Deserialize)]
struct RefreshBody {
    refresh_token: String,
}

async fn refresh(
    State(api): State<Api>,
    JsonBody(body): JsonBody<RefreshBody>,
) -> Result<Response, AuthError> {
    let tokens = api.auth.refresh(&body.refresh_token).await?;
    Ok(tokens_response(StatusCode::OK, &tokens))
}

async fn forgot(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<EmailBody>,
) -> Result<Response, AuthError> {
    api.limits
        .check(Action::ForgotPassword, ip, Some(&body.email))
        .await?;
    api.auth.forgot_password(&body.email).await?;
    Ok(check_email())
}

async fn reset_page(Query(query): Query<TokenQuery>) -> Response {
    page(
        StatusCode::OK,
        "Choose a new password",
        &format!(
            r#"<form method="post" action="/v1/auth/password/reset"><input type="hidden" name="token" value="{}"><label>New password <input type="password" name="password" minlength="8" maxlength="128" autocomplete="new-password" required></label> <button type="submit">Set password</button></form>"#,
            escape(&query.token)
        ),
    )
}

#[derive(Deserialize)]
struct ResetBody {
    token: String,
    password: String,
}

async fn reset(
    State(api): State<Api>,
    ClientIp(ip): ClientIp,
    body: Submitted<ResetBody>,
) -> Result<Response, AuthError> {
    api.limits.check(Action::OpenLink, ip, None).await?;
    let (body, as_page) = match body {
        Submitted::Json(body) => (body, false),
        Submitted::Form(body) => (body, true),
    };
    let user = match api.auth.reset_password(&body.token, &body.password).await {
        Ok(user) => user,
        Err(error) if as_page => return Ok(error_page(&error)),
        Err(error) => return Err(error),
    };
    let mut response = if as_page {
        page(
            StatusCode::OK,
            "Password changed",
            "<p>Your password is changed and every device is signed out. Sign in again with the new password.</p>",
        )
    } else {
        Json(json!({"status": "password_changed"})).into_response()
    };
    response.extensions_mut().insert(Subject {
        user: Some(user),
        device: None,
    });
    Ok(response)
}

async fn jwks(State(api): State<Api>) -> Response {
    let mut response = Json(api.auth.tokens().jwks()).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    response
}

#[derive(Deserialize)]
struct DeviceBody {
    name: String,
    platform: String,
    /// The client's idempotency key for this registration; its retries send it again.
    registration_id: Uuid,
}

async fn register_device(
    State(api): State<Api>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<DeviceBody>,
) -> Result<Response, AuthError> {
    let tokens = api
        .auth
        .register_device(
            bearer(&headers)?,
            body.registration_id,
            &body.name,
            &body.platform,
        )
        .await?;
    Ok(tokens_response(StatusCode::CREATED, &tokens))
}

async fn list_devices(State(api): State<Api>, headers: HeaderMap) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let devices: Vec<Value> = api
        .auth
        .list_devices(caller)
        .await?
        .into_iter()
        .map(|device| {
            json!({
                "device_id": device.device_id,
                "name": device.name,
                "platform": device.platform.as_str(),
                "created_at": device.created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                "last_seen_at": device
                    .last_seen_at
                    .map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true)),
                "current": device.device_id == caller.device_id,
            })
        })
        .collect();
    Ok(Json(json!({"devices": devices})).into_response())
}

async fn revoke_device(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let device_id: Uuid = device_id.parse().map_err(|_| AuthError::DeviceNotFound)?;
    api.auth.revoke_device(caller, device_id).await?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.extensions_mut().insert(Subject {
        user: Some(caller.user_id),
        device: Some(device_id),
    });
    Ok(response)
}

fn own_profile_json(profile: &crate::profiles::OwnProfile) -> Value {
    json!({
        "user_id": profile.user_id,
        "handle": profile.handle,
        "display_name": profile.display_name,
        "email": profile.email,
        "kind": profile.kind,
        "owner_user_id": profile.owner_user_id,
    })
}

async fn own_profile(State(api): State<Api>, headers: HeaderMap) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let profile = api.auth.own_profile(caller).await?;
    Ok(Json(own_profile_json(&profile)).into_response())
}

#[derive(Deserialize)]
struct ProfileBody {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    handle: Option<String>,
}

/// `PATCH /v1/users/me {display_name?, handle?}`: the access token is the only proof
/// needed (spec D14); a handle change is rate limited per IP and per user.
async fn update_profile(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    JsonBody(body): JsonBody<ProfileBody>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    if body.handle.is_some() {
        let user = caller.user_id.to_string();
        api.limits
            .check(Action::HandleChange, ip, Some(&user))
            .await?;
    }
    let profile = api
        .auth
        .update_profile(caller, body.display_name.as_deref(), body.handle.as_deref())
        .await?;
    let mut response = Json(own_profile_json(&profile)).into_response();
    response.extensions_mut().insert(Subject {
        user: Some(caller.user_id),
        device: Some(caller.device_id),
    });
    Ok(response)
}

#[derive(Deserialize)]
struct HandleQuery {
    #[serde(default)]
    handle: String,
}

async fn lookup(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Query(query): Query<HandleQuery>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let user = caller.user_id.to_string();
    api.limits
        .check(Action::HandleLookup, ip, Some(&user))
        .await?;
    let profile = api.auth.lookup(&query.handle).await?;
    Ok(Json(json!({
        "user_id": profile.user_id,
        "handle": profile.handle,
        "display_name": profile.display_name,
        "kind": profile.kind,
        "owner_user_id": profile.owner_user_id,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct IdsQuery {
    #[serde(default)]
    ids: String,
}

/// `GET /v1/users?ids=…`: the profiles of the listed users that the caller shares a
/// contact or a conversation with (message-svc decides), and the caller's own. Others are
/// left out, as are unknown ids: the answer never tells which ids exist.
async fn profiles(
    State(api): State<Api>,
    headers: HeaderMap,
    Query(query): Query<IdsQuery>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let ids = rules::user_ids(&query.ids)?;
    let mut visible = api.directory.visible_users(caller.user_id, &ids).await?;
    // Defence in depth: never show a profile that was not asked for, whatever the answer.
    visible.retain(|user| ids.binary_search(user).is_ok());
    let profiles = api.auth.profiles(&visible).await?;
    let users: Vec<Value> = profiles
        .iter()
        .map(|profile| {
            json!({
                "user_id": profile.user_id,
                "handle": profile.handle,
                "display_name": profile.display_name,
                "kind": profile.kind,
                "owner_user_id": profile.owner_user_id,
            })
        })
        .collect();
    Ok(Json(json!({ "users": users })).into_response())
}

/// A small HTML page with the headers every link page needs.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title}</title></head><body><main><h1>{title}</h1>{body}</main></body></html>"#,
        title = escape(title),
    );
    let mut response = (status, Html(html)).into_response();
    let headers = response.headers_mut();
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; form-action 'self'; frame-ancestors 'none'"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

fn error_page(error: &AuthError) -> Response {
    let status = StatusCode::from_u16(error.status()).expect("a valid status");
    let message = match error {
        AuthError::LinkExpired => "This link has expired. Ask for a new one.".to_owned(),
        AuthError::LinkInvalid => "This link is not valid, or it was used already.".to_owned(),
        other => other.to_string(),
    };
    let mut response = page(
        status,
        "Something went wrong",
        &format!("<p>{}</p>", escape(&message)),
    );
    response.extensions_mut().insert(ErrorCode(error.code()));
    response
}

/// Escapes text for HTML content and attribute values.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_markup_and_quotes() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn errors_answer_with_their_status_code_and_headers() {
        let response = AuthError::RateLimited { retry_after_s: 42 }.into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "42");
        let response = AuthError::Unauthorized.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        let response = AuthError::EntraUnavailable.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "5");
        let response = AuthError::SessionExpired.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
    }

    #[test]
    fn identity_refusals_answer_their_codes() {
        for (error, status, code) in [
            (AuthError::TokenInvalid, 401, "token_invalid"),
            (AuthError::IdentityUnavailable, 503, "identity_unavailable"),
            (AuthError::SignInNotPermitted, 403, "sign_in_not_permitted"),
        ] {
            assert_eq!((error.status(), error.code()), (status, code));
        }
        let response = AuthError::TokenInvalid.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[WWW_AUTHENTICATE], "Bearer");
        let response = AuthError::IdentityUnavailable.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[RETRY_AFTER], "5");
        assert!(AuthError::IdentityUnavailable.retryable());
        assert!(!AuthError::SignInNotPermitted.retryable());
        assert!(!AuthError::TokenInvalid.retryable());
    }

    #[test]
    fn the_client_ip_is_the_last_entry_the_proxy_wrote() {
        let parsed = |value: &str| ip_from_header(value).map(|ip| ip.to_string());
        assert_eq!(parsed("203.0.113.7").as_deref(), Some("203.0.113.7"));
        // A client-supplied first entry is ignored.
        assert_eq!(
            parsed("10.9.9.9, 198.51.100.4").as_deref(),
            Some("198.51.100.4")
        );
        assert_eq!(parsed(" 2001:db8::1 ").as_deref(), Some("2001:db8::1"));
        assert_eq!(parsed("198.51.100.4, unknown"), None);
        assert_eq!(parsed(""), None);
    }
}
