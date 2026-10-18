//! Security events: one structured log line, under the `security` target, for every
//! security-relevant outcome of the public API — sign-ins (email and password, or Entra
//! ID), refused tokens, rate limits, bad links, password resets, device changes. A
//! middleware classifies each answer by its route, status and error code, so no handler
//! can forget to log.
//!
//! The line carries the event name, the route, the status, the error code, the client IP
//! and, where the answer knows them, the user and device ids; never an email address, a
//! password or a token. Failures log at `warn`, successes at `info`.

use std::net::IpAddr;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::api::{Api, client_ip};

/// The error code of a refused request ([`crate::AuthError::code`]), attached to its
/// answer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ErrorCode(pub &'static str);

/// Whose account or device a request concerned, attached to its answer by the handler
/// that knows.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Subject {
    pub user: Option<Uuid>,
    pub device: Option<Uuid>,
}

/// Logs the security event of every answer that has one.
pub(crate) async fn audit(State(api): State<Api>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_default();
    let ip = client_ip(&api, request.headers(), request.extensions());
    let response = next.run(request).await;
    let status = response.status();
    let code = response.extensions().get::<ErrorCode>().map(|code| code.0);
    if let Some(event) = event(&method, &route, status, code) {
        let subject = response
            .extensions()
            .get::<Subject>()
            .copied()
            .unwrap_or_default();
        log(event, &route, status, code, ip, subject);
    }
    response
}

fn log(
    event: &'static str,
    route: &str,
    status: StatusCode,
    code: Option<&str>,
    ip: Option<IpAddr>,
    subject: Subject,
) {
    let status = status.as_u16();
    let ip = ip.map(tracing::field::display);
    let user_id = subject.user.map(tracing::field::display);
    let device_id = subject.device.map(tracing::field::display);
    if (200..300).contains(&status) {
        tracing::info!(target: "security", event, route, status, code, ip, user_id, device_id, "security event");
    } else {
        tracing::warn!(target: "security", event, route, status, code, ip, user_id, device_id, "security event");
    }
}

/// The event of an answer, if it is security-relevant. Unavailable answers (503) are
/// not: they say nothing about the caller.
pub(crate) fn event(
    method: &Method,
    route: &str,
    status: StatusCode,
    code: Option<&str>,
) -> Option<&'static str> {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Some("auth.rate_limited");
    }
    if status == StatusCode::SERVICE_UNAVAILABLE {
        return None;
    }
    let ok = status.is_success();
    let bad_link = matches!(code, Some("link_invalid" | "link_expired"));
    let event = match (method.as_str(), route) {
        ("POST", "/v1/auth/signup") if ok => "auth.signup",
        ("POST", "/v1/auth/login") if ok => "auth.login.success",
        ("POST", "/v1/auth/login") if status != StatusCode::BAD_REQUEST => "auth.login.failure",
        ("POST", "/v1/auth/entra") if ok => "auth.entra.success",
        ("POST", "/v1/auth/entra") if status != StatusCode::BAD_REQUEST => "auth.entra.failure",
        ("POST", "/v1/auth/refresh") if ok => return None,
        ("POST", "/v1/auth/refresh") => "auth.refresh.refused",
        ("POST", "/v1/auth/verify") if ok => "auth.email.verified",
        ("POST", "/v1/auth/password/forgot") if ok => "auth.password.forgot",
        ("POST", "/v1/auth/password/reset") if ok => "auth.password.reset",
        ("POST", "/v1/auth/verify" | "/v1/auth/password/reset") if bad_link => "auth.link.invalid",
        ("POST", "/v1/devices") if ok => "auth.device.registered",
        ("POST", "/v1/devices") if code == Some("too_many_devices") => "auth.device.limit",
        ("DELETE", "/v1/devices/{device_id}") if ok => "auth.device.revoked",
        _ if status == StatusCode::UNAUTHORIZED => "auth.token.refused",
        _ => return None,
    };
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_classified_by_route_status_and_code() {
        let post = Method::POST;
        let cases = [
            (
                &post,
                "/v1/auth/entra",
                200,
                None,
                Some("auth.entra.success"),
            ),
            (
                &post,
                "/v1/auth/entra",
                401,
                Some("entra_token_invalid"),
                Some("auth.entra.failure"),
            ),
            (&post, "/v1/auth/entra", 400, Some("invalid"), None),
            (
                &post,
                "/v1/auth/entra",
                503,
                Some("entra_unavailable"),
                None,
            ),
            (
                &post,
                "/v1/auth/refresh",
                401,
                Some("session_expired"),
                Some("auth.refresh.refused"),
            ),
            (
                &post,
                "/v1/auth/login",
                200,
                None,
                Some("auth.login.success"),
            ),
            (
                &post,
                "/v1/auth/login",
                401,
                Some("invalid_credentials"),
                Some("auth.login.failure"),
            ),
            (
                &post,
                "/v1/auth/login",
                403,
                Some("email_not_verified"),
                Some("auth.login.failure"),
            ),
            (&post, "/v1/auth/login", 400, Some("invalid"), None),
            (
                &post,
                "/v1/auth/login",
                429,
                Some("rate_limited"),
                Some("auth.rate_limited"),
            ),
            (&post, "/v1/auth/login", 503, Some("unavailable"), None),
            (&post, "/v1/auth/refresh", 200, None, None),
            (
                &post,
                "/v1/auth/refresh",
                401,
                Some("unauthorized"),
                Some("auth.refresh.refused"),
            ),
            (
                &post,
                "/v1/auth/password/reset",
                200,
                None,
                Some("auth.password.reset"),
            ),
            (
                &post,
                "/v1/auth/password/reset",
                400,
                Some("link_expired"),
                Some("auth.link.invalid"),
            ),
            (
                &post,
                "/v1/auth/verify",
                400,
                Some("link_invalid"),
                Some("auth.link.invalid"),
            ),
            (
                &post,
                "/v1/devices",
                201,
                None,
                Some("auth.device.registered"),
            ),
            (
                &post,
                "/v1/devices",
                409,
                Some("too_many_devices"),
                Some("auth.device.limit"),
            ),
            (
                &Method::DELETE,
                "/v1/devices/{device_id}",
                204,
                None,
                Some("auth.device.revoked"),
            ),
            (
                &Method::GET,
                "/v1/users/me",
                401,
                Some("token_expired"),
                Some("auth.token.refused"),
            ),
            (&Method::GET, "/v1/users/me", 200, None, None),
        ];
        for (method, route, status, code, expected) in cases {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                event(method, route, status, code),
                expected,
                "{method} {route} {status}"
            );
        }
    }
}
