//! The identity middleware (spec 2026-10-19 pluggable identity, D5) on
//! `POST /v1/auth/{provider}`: (1) the per-IP sign-in limit; (2) the token from
//! `Authorization: Bearer`, else the JSON body's `token` (the body, at most 16 KiB, is handed
//! on unchanged; both present and different is `invalid`); (3) the authenticator; (4) the
//! admission; (5) the principal in the request's extensions; (6) the per-identity limit.
//! Refusals are logged here for every authenticator, never with the token, the subject,
//! the name or the address.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{Map, Value};

use super::{
    AdmissionDecision, AuthenticationError, Authenticator, Principal, admission_unavailable,
    not_permitted, rejected, unavailable,
};
use crate::api::{Api, MAX_BODY_BYTES, bad_body, client_ip};
use crate::error::AuthError;
use crate::limits::Action;
use crate::rules::Invalid;

/// The longest token accepted from the header or the body, in bytes.
const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// What the middleware of one sign-in route needs.
#[derive(Clone)]
pub(crate) struct IdentityState {
    pub api: Api,
    pub authenticator: Arc<dyn Authenticator>,
}

/// Runs steps (1) to (6), then the handler with the principal in the request.
pub(crate) async fn identify(
    State(state): State<IdentityState>,
    request: Request,
    next: Next,
) -> Result<Response, AuthError> {
    let IdentityState { api, authenticator } = state;
    let provider = authenticator.provider();
    let ip = client_ip(&api, request.headers(), request.extensions());
    api.limits.check(Action::EntraSignIn, ip, None).await?;
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| bad_body())?;
    if !is_json(&parts.headers) {
        return Err(bad_body());
    }
    let object: Map<String, Value> = serde_json::from_slice(&bytes).map_err(|_| bad_body())?;
    let in_body = match object.get("token") {
        None => None,
        Some(Value::String(token)) => Some(token.as_str()),
        Some(_) => return Err(bad_body()),
    };
    let token = match (header_token(&parts.headers)?, in_body) {
        (Some(header), Some(body)) if header != body => {
            return Err(invalid(
                "token",
                "the Authorization header and the body carry different tokens",
            ));
        }
        (Some(header), _) => header,
        (None, Some(body)) => body.to_owned(),
        (None, None) => return Err(bad_body()),
    };
    if token.len() > MAX_TOKEN_BYTES {
        return Err(invalid("token", "at most 16 KiB"));
    }
    let principal = match authenticator.authenticate(&token).await {
        Ok(principal) => principal
            .checked(provider)
            .ok_or_else(|| rejected(provider, "principal"))?,
        Err(AuthenticationError::Rejected(reason)) => return Err(rejected(provider, reason)),
        Err(AuthenticationError::Unavailable(_)) => return Err(unavailable(provider)),
    };
    if let Some(admission) = api.auth.admission() {
        match admission.admit(&principal).await {
            AdmissionDecision::Admit => {}
            AdmissionDecision::Refuse => return Err(not_permitted(&principal)),
            AdmissionDecision::Unavailable => return Err(admission_unavailable(provider)),
        }
    }
    let account = limit_key(&principal);
    let mut request = Request::from_parts(parts, Body::from(bytes));
    request.extensions_mut().insert(principal);
    api.limits
        .check(Action::EntraAccount, None, Some(&account))
        .await?;
    Ok(next.run(request).await)
}

fn invalid(field: &'static str, reason: &'static str) -> AuthError {
    AuthError::Invalid(Invalid { field, reason })
}

/// The token of `Authorization: Bearer <token>`: none without the header; `invalid` for
/// any other form or more than one header.
fn header_token(headers: &HeaderMap) -> Result<Option<String>, AuthError> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(invalid("authorization", "give one Authorization header"));
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| Some(token.to_owned()))
        .ok_or_else(|| invalid("authorization", "expected Bearer and a token"))
}

/// Whether the request says its body is JSON, as axum's `Json` requires.
fn is_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "application/json"
        || (essence.starts_with("application/") && essence.ends_with("+json"))
}

/// The per-identity rate-limit key: `{provider}:{issuer}:{subject}`, with `%` and `:` of
/// the issuer escaped so no two identities share a key by their separators. Entra ID's
/// keys stay `entra:{tid}:{oid}`.
fn limit_key(principal: &Principal) -> String {
    let issuer = principal.issuer.replace('%', "%25").replace(':', "%3A");
    format!("{}:{issuer}:{}", principal.provider, principal.subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(provider: &str, issuer: &str, subject: &str) -> Principal {
        Principal {
            provider: provider.to_owned(),
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
            name: None,
            email: None,
            claims: Map::new(),
        }
    }

    #[test]
    fn identities_never_share_a_rate_limit_key_by_their_separators() {
        let tenant = "33333333-0000-4000-8000-000000000003";
        let oid = "55555555-0000-4000-8000-000000000005";
        assert_eq!(
            limit_key(&principal("entra", tenant, oid)),
            format!("entra:{tenant}:{oid}")
        );
        assert_ne!(
            limit_key(&principal("acme", "a:b", "c")),
            limit_key(&principal("acme", "a", "b:c"))
        );
        assert_ne!(
            limit_key(&principal("acme", "a%3Ab", "c")),
            limit_key(&principal("acme", "a:b", "c"))
        );
    }
}
