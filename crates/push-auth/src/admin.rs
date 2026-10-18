//! The admin API (spec D15): `POST /v1/admin/users/revoke-sessions` signs a user out of
//! every device. It is served only on its own listener (`PUSH_AUTH_ADMIN_ADDR`), and only
//! when `PUSH_AUTH_ADMIN_TOKEN` is set; the chart never routes it through the Ingress. The
//! token is compared in constant time over SHA-256 digests and never logged. Calls with the
//! token are rate limited per IP; failed attempts have a limit of their own per IP, so they
//! never use up the token holder's.

use std::net::SocketAddr;

use axum::body::to_bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderValue;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::AuthError;
use crate::limits::{Action, Limits};
use crate::rules::{self, Invalid};
use crate::service::Auth;

/// Largest request body.
const MAX_BODY_BYTES: usize = 4 * 1024;

/// Whom to sign out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserSelector {
    Id(Uuid),
    /// A normalised handle.
    Handle(String),
    Entra {
        tenant_id: Uuid,
        object_id: Uuid,
    },
}

impl UserSelector {
    /// The kind, for the security log (never the value).
    fn kind(&self) -> &'static str {
        match self {
            Self::Id(_) => "user_id",
            Self::Handle(_) => "handle",
            Self::Entra { .. } => "entra",
        }
    }
}

/// What the admin handlers share.
#[derive(Clone)]
pub struct AdminApi {
    auth: Auth,
    limits: Limits,
    /// SHA-256 of `PUSH_AUTH_ADMIN_TOKEN`.
    token: [u8; 32],
}

impl std::fmt::Debug for AdminApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AdminApi(<redacted>)")
    }
}

impl AdminApi {
    pub fn new(auth: Auth, limits: Limits, token: &str) -> Self {
        Self {
            auth,
            limits,
            token: Sha256::digest(token.as_bytes()).into(),
        }
    }

    /// Whether `presented` is the admin token; constant time over the digests, so neither
    /// the content nor the length leaks.
    fn authorized(&self, presented: Option<&str>) -> bool {
        let Some(presented) = presented else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        digest
            .iter()
            .zip(&self.token)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
    }
}

/// The admin router; serve it with `into_make_service_with_connect_info::<SocketAddr>()`.
pub fn admin_router(admin: AdminApi) -> Router {
    Router::new()
        .route("/v1/admin/users/revoke-sessions", post(revoke_sessions))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response
                    .headers_mut()
                    .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
            },
        ))
        .with_state(admin)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeBody {
    #[serde(default)]
    user_id: Option<Uuid>,
    #[serde(default)]
    handle: Option<String>,
    #[serde(default)]
    entra: Option<EntraSubject>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntraSubject {
    tenant_id: Uuid,
    object_id: Uuid,
}

fn invalid(reason: &'static str) -> AuthError {
    AuthError::Invalid(Invalid {
        field: "body",
        reason,
    })
}

/// `POST /v1/admin/users/revoke-sessions` with exactly one of `{user_id}`, `{handle}`,
/// `{entra: {tenant_id, object_id}}`: `{user_id, devices_revoked}`.
async fn revoke_sessions(
    State(admin): State<AdminApi>,
    request: Request,
) -> Result<Response, AuthError> {
    let ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip());
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    if !admin.authorized(presented) {
        tracing::warn!(
            target: "security",
            event = "auth.admin.token.refused",
            ip = ip.map(tracing::field::display),
            token_presented = presented.is_some(),
            "an admin call was refused"
        );
        // Failed attempts are counted on their own, so callers without the token can lock
        // only their own guesses out, never the token's holder.
        admin
            .limits
            .check(Action::AdminAuthFailure, ip, None)
            .await?;
        return Err(AuthError::Unauthorized);
    }
    admin.limits.check(Action::Admin, ip, None).await?;
    let bytes = to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| invalid("expected a JSON object of at most 4 KiB"))?;
    let body: RevokeBody = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("expected exactly one of user_id, handle or entra"))?;
    let selector = match (body.user_id, body.handle, body.entra) {
        (Some(id), None, None) => UserSelector::Id(id),
        (None, Some(handle), None) => UserSelector::Handle(rules::handle(&handle)?),
        (None, None, Some(entra)) => UserSelector::Entra {
            tenant_id: entra.tenant_id,
            object_id: entra.object_id,
        },
        _ => return Err(invalid("give exactly one of user_id, handle or entra")),
    };
    let user_id = admin.auth.find_user(&selector).await?;
    let revoked = admin.auth.revoke_user_sessions(user_id).await?;
    tracing::warn!(
        target: "security",
        event = "auth.admin.sessions_revoked",
        %user_id,
        devices = revoked,
        selector = selector.kind(),
        ip = ip.map(tracing::field::display),
        "an operator signed a user out of every device"
    );
    Ok(Json(json!({"user_id": user_id, "devices_revoked": revoked})).into_response())
}

impl Auth {
    /// The account `selector` names; `UserNotFound` if none.
    pub async fn find_user(&self, selector: &UserSelector) -> Result<Uuid, AuthError> {
        let found: Option<Uuid> = match selector {
            UserSelector::Id(id) => {
                sqlx::query_scalar("SELECT user_id FROM auth.user_account WHERE user_id = $1")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?
            }
            UserSelector::Handle(handle) => {
                sqlx::query_scalar(
                    "SELECT user_id FROM auth.user_account WHERE handle = $1::citext",
                )
                .bind(handle)
                .fetch_optional(&self.pool)
                .await?
            }
            UserSelector::Entra {
                tenant_id,
                object_id,
            } => {
                sqlx::query_scalar(
                    "SELECT user_id FROM auth.external_identity \
                     WHERE provider = 'entra' AND tenant_id = $1 AND subject = $2",
                )
                .bind(tenant_id)
                .bind(object_id.to_string())
                .fetch_optional(&self.pool)
                .await?
            }
        };
        found.ok_or(AuthError::UserNotFound)
    }
}
