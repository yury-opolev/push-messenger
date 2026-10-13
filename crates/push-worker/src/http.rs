//! The worker's HTTP API: Resend's delivery webhooks and the DEV ONLY mailbox.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use crate::deliveries::{Deliveries, DeliveryError};
use crate::email::Mailbox;

/// The only path the ingress routes to the worker.
pub const WEBHOOK_PATH: &str = "/v1/webhooks/resend";
/// Webhook bodies are small JSON documents.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// What the API serves: webhooks when a webhook secret is configured, the mailbox in dev.
#[derive(Debug, Clone, Default)]
pub struct Http {
    pub deliveries: Option<Deliveries>,
    /// DEV ONLY: `GET /dev/mailbox?to=…`.
    pub mailbox: Option<Mailbox>,
}

/// The router: `POST /v1/webhooks/resend` and, when enabled, `GET /dev/mailbox`.
pub fn router(http: Http) -> Router {
    let mut router = Router::new();
    if let Some(deliveries) = http.deliveries {
        router = router.route(
            WEBHOOK_PATH,
            post(webhook)
                .with_state(deliveries)
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        );
    }
    if let Some(mailbox) = http.mailbox {
        router = router.route("/dev/mailbox", get(dev_mailbox).with_state(mailbox));
    }
    router
}

/// 204 once recorded (or a replay, an event nothing acts on, or a signed body that is not
/// a delivery event, which is logged and dropped); 401 for a missing or wrong signature or
/// a stale timestamp; 503 when the database did not answer. Resend sends anything not
/// answered 2xx again, for days, so only what a retry could fix is not a 2xx.
async fn webhook(
    State(deliveries): State<Deliveries>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    match deliveries.receive(&header, &body).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(DeliveryError::Refused(error)) => {
            tracing::warn!(%error, "webhook refused");
            (StatusCode::UNAUTHORIZED, error.to_string()).into_response()
        }
        Err(error @ DeliveryError::Database(_)) => {
            tracing::warn!(%error, "webhook not recorded");
            (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
        }
    }
}

#[derive(Deserialize)]
struct MailboxQuery {
    #[serde(default)]
    to: String,
}

/// DEV ONLY: the emails the log sender delivered to one address, newest first.
async fn dev_mailbox(
    State(mailbox): State<Mailbox>,
    Query(query): Query<MailboxQuery>,
) -> Response {
    let emails: Vec<_> = mailbox
        .to(&query.to)
        .into_iter()
        .map(|email| {
            json!({
                "template": email.template,
                "subject": email.content.subject,
                "text": email.content.text,
            })
        })
        .collect();
    Json(json!({"emails": emails})).into_response()
}
