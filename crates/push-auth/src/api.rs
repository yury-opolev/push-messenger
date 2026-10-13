//! The public HTTP API (spec §2, §5): `/v1/auth/*`, `/v1/devices/*` and `/v1/users/*`,
//! JSON in and out, plus the two small pages that email links open.
//!
//! Errors are `{"error": {"code", "message", "retryable"}}` with the status of
//! [`AuthError::status`]; 429 answers carry `Retry-After`, 401 answers
//! `WWW-Authenticate: Bearer`. No answer may be cached (`Cache-Control: no-store`) except
//! the JWKS. Pages send `Referrer-Policy: no-referrer`, so the token in their URL never
//! leaks to another site (spec §5.6), and change nothing on `GET`: the change is the
//! `POST` their button sends, so a mail scanner that opens links does nothing.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    REFERRER_POLICY, RETRY_AFTER, WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::request::Parts;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Json, Router};
use chrono::SecondsFormat;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::devices::{Caller, Tokens};
use crate::error::AuthError;
use crate::limits::{Action, Limits};
use crate::rules::Invalid;
use crate::service::{Auth, Signup};

/// Largest request body accepted.
const MAX_BODY_BYTES: usize = 16 * 1024;

/// What the handlers share.
#[derive(Debug, Clone)]
pub struct Api {
    pub auth: Auth,
    pub limits: Limits,
    /// The header that carries the client's IP, set by the ingress controller
    /// (`x-real-ip`); without it, the peer address. Only trust a header the proxy in front
    /// always overwrites (or, for `X-Forwarded-For`, appends to): its last entry is used.
    pub client_ip_header: Option<HeaderName>,
}

/// The router of the public API.
pub fn router(api: Api) -> Router {
    Router::new()
        .route("/v1/auth/signup", post(signup))
        .route("/v1/auth/verify", get(verify_page).post(verify))
        .route("/v1/auth/verify/resend", post(resend))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/password/forgot", post(forgot))
        .route("/v1/auth/password/reset", get(reset_page).post(reset))
        .route("/v1/auth/jwks", get(jwks))
        .route("/v1/devices", post(register_device).get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke_device))
        .route("/v1/users/me", get(own_profile).patch(set_display_name))
        .route("/v1/users/lookup", get(lookup))
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
        let headers = response.headers_mut();
        match self {
            Self::RateLimited { retry_after_s } => {
                headers.insert(RETRY_AFTER, HeaderValue::from(retry_after_s));
            }
            Self::Unavailable => {
                headers.insert(RETRY_AFTER, HeaderValue::from_static("1"));
            }
            Self::Unauthorized | Self::TokenExpired | Self::InvalidCredentials => {
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

fn bad_body() -> AuthError {
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
        let from_header = api.client_ip_header.as_ref().and_then(|name| {
            // The last line: a proxy that appends its own line instead of joining it must
            // not let the client's line win.
            let value = parts
                .headers
                .get_all(name)
                .iter()
                .next_back()?
                .to_str()
                .ok()?;
            ip_from_header(value)
        });
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip());
        Ok(Self(from_header.or(peer)))
    }
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
            Json(tokens_json(&tokens)).into_response()
        }
    })
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
    Ok(Json(tokens_json(&tokens)).into_response())
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
    match body {
        Submitted::Json(body) => {
            api.auth.reset_password(&body.token, &body.password).await?;
            Ok(Json(json!({"status": "password_changed"})).into_response())
        }
        Submitted::Form(body) => Ok(
            match api.auth.reset_password(&body.token, &body.password).await {
                Ok(()) => page(
                    StatusCode::OK,
                    "Password changed",
                    "<p>Your password is changed and every device is signed out. Sign in again with the new password.</p>",
                ),
                Err(error) => error_page(&error),
            },
        ),
    }
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
    Ok((StatusCode::CREATED, Json(tokens_json(&tokens))).into_response())
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
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn own_profile_json(profile: &crate::profiles::OwnProfile) -> Value {
    json!({
        "user_id": profile.user_id,
        "handle": profile.handle,
        "display_name": profile.display_name,
        "email": profile.email,
    })
}

async fn own_profile(State(api): State<Api>, headers: HeaderMap) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let profile = api.auth.own_profile(caller).await?;
    Ok(Json(own_profile_json(&profile)).into_response())
}

#[derive(Deserialize)]
struct DisplayNameBody {
    display_name: String,
}

async fn set_display_name(
    State(api): State<Api>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<DisplayNameBody>,
) -> Result<Response, AuthError> {
    let caller = caller(&api, &headers).await?;
    let profile = api
        .auth
        .set_display_name(caller, &body.display_name)
        .await?;
    Ok(Json(own_profile_json(&profile)).into_response())
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
    }))
    .into_response())
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
    page(
        status,
        "Something went wrong",
        &format!("<p>{}</p>", escape(&message)),
    )
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
