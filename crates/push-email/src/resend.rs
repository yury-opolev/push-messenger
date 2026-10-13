//! The Resend sender (spec §9): `POST /emails` with the email's id as the idempotency key.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::sender::{EmailSender, OutgoingEmail, SendError};

/// Resend's API.
pub const RESEND_API_URL: &str = "https://api.resend.com";

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Sends through Resend's HTTP API. Open and click tracking are settings of the sending
/// domain at Resend, off for ours (spec §9).
#[derive(Clone)]
pub struct ResendSender {
    http: reqwest::Client,
    emails_url: String,
    api_key: String,
    from: String,
}

impl std::fmt::Debug for ResendSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResendSender")
            .field("emails_url", &self.emails_url)
            .field("api_key", &"<redacted>")
            .field("from", &self.from)
            .finish()
    }
}

impl ResendSender {
    /// A sender that posts to `{api_url}/emails` ([`RESEND_API_URL`] in production) with
    /// `api_key`, from the address `from` (`Push Messenger <no-reply@push.kubegames.dk>`).
    pub fn new(api_url: &str, api_key: String, from: String) -> Result<Self, SendError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| SendError::Permanent("could not build the HTTP client".to_owned()))?;
        Ok(Self {
            http,
            emails_url: format!("{}/emails", api_url.trim_end_matches('/')),
            api_key,
            from,
        })
    }
}

#[async_trait]
impl EmailSender for ResendSender {
    async fn send(
        &self,
        email: &OutgoingEmail,
        idempotency_key: Uuid,
    ) -> Result<String, SendError> {
        let body = json!({
            "from": self.from,
            "to": [email.to],
            "subject": email.content.subject,
            "text": email.content.text,
            "html": email.content.html,
            "tags": [{"name": "template", "value": email.template}],
        });
        let response = self
            .http
            .post(&self.emails_url)
            .bearer_auth(&self.api_key)
            .header("Idempotency-Key", idempotency_key.to_string())
            .json(&body)
            .send()
            .await
            // Without the URL or the request: neither belongs in a log.
            .map_err(|_| SendError::Retryable("Resend could not be reached".to_owned()))?;
        let status = response.status();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return answer["id"].as_str().map(str::to_owned).ok_or_else(|| {
                SendError::Retryable(format!("Resend answered {status} without an id"))
            });
        }
        // Only Resend's error name: its message can quote the address.
        let name: String = answer["name"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| c.is_ascii_lowercase() || *c == '_')
            .take(64)
            .collect();
        let reason = format!("Resend answered {status} ({name})");
        if is_retryable(status) {
            Err(SendError::Retryable(reason))
        } else {
            Err(SendError::Permanent(reason))
        }
    }
}

/// Rate limits, a timeout, a concurrent request with the same idempotency key, server
/// errors, and a missing or wrong API key (401, 403): the emails wait while the key is fixed.
fn is_retryable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::REQUEST_TIMEOUT
            | StatusCode::CONFLICT
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
    ) || status.is_server_error()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode as AxumStatus};
    use axum::routing::post;
    use tokio::net::TcpListener;

    use super::*;
    use crate::templates::Rendered;

    /// What the stub received: the headers and the body of each request.
    type Received = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

    /// A stand-in for Resend that answers every request with `status` and `answer`.
    async fn stub(status: u16, answer: Value) -> (String, Received) {
        let received: Received = Arc::default();
        let app = Router::new()
            .route(
                "/emails",
                post(
                    move |State(received): State<Received>,
                          headers: HeaderMap,
                          Json(body): Json<Value>| {
                        let answer = answer.clone();
                        async move {
                            received.lock().unwrap().push((headers, body));
                            (AxumStatus::from_u16(status).unwrap(), Json(answer))
                        }
                    },
                ),
            )
            .with_state(Arc::clone(&received));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, received)
    }

    fn email() -> OutgoingEmail {
        OutgoingEmail {
            to: "alice@example.com".to_owned(),
            template: "verify_email".to_owned(),
            content: Rendered {
                subject: "Verify your email address".to_owned(),
                text: "Open https://x.test/?token=t".to_owned(),
                html: "<p>Open</p>".to_owned(),
            },
        }
    }

    fn sender(url: &str) -> ResendSender {
        ResendSender::new(
            url,
            "re_test_key".to_owned(),
            "Push <no-reply@push.test>".to_owned(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn an_email_is_posted_with_the_key_and_the_idempotency_key() {
        let (url, received) = stub(200, json!({"id": "re-123"})).await;
        let key = Uuid::now_v7();
        assert_eq!(
            sender(&format!("{url}/")).send(&email(), key).await,
            Ok("re-123".to_owned())
        );

        let received = received.lock().unwrap();
        let (headers, body) = &received[0];
        assert_eq!(headers["authorization"], "Bearer re_test_key");
        assert_eq!(headers["idempotency-key"], key.to_string().as_str());
        assert_eq!(
            body,
            &json!({
                "from": "Push <no-reply@push.test>",
                "to": ["alice@example.com"],
                "subject": "Verify your email address",
                "text": "Open https://x.test/?token=t",
                "html": "<p>Open</p>",
                "tags": [{"name": "template", "value": "verify_email"}],
            })
        );
    }

    #[tokio::test]
    async fn rate_limits_key_problems_and_outages_are_retried_refusals_are_not() {
        for status in [401, 403, 409, 429, 500, 503] {
            let (url, _) = stub(status, json!({"name": "busy"})).await;
            let error = sender(&url).send(&email(), Uuid::now_v7()).await;
            assert!(
                matches!(error, Err(SendError::Retryable(_))),
                "{status}: {error:?}"
            );
        }
        let refusal = json!({
            "statusCode": 422,
            "name": "validation_error",
            "message": "Invalid `to` field: alice@example.com",
        });
        let (url, _) = stub(422, refusal).await;
        let error = sender(&url)
            .send(&email(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            SendError::Permanent(
                "Resend answered 422 Unprocessable Entity (validation_error)".to_owned()
            )
        );
        assert!(!error.to_string().contains("alice"));
    }

    #[tokio::test]
    async fn an_unreachable_api_is_retried_and_not_named() {
        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", unused.local_addr().unwrap());
        drop(unused);
        let error = sender(&url)
            .send(&email(), Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(matches!(error, SendError::Retryable(_)), "{error:?}");
        assert!(!error.to_string().contains("127.0.0.1"), "{error}");
    }

    #[tokio::test]
    async fn a_success_without_an_id_is_retried() {
        let (url, _) = stub(200, json!({})).await;
        assert!(matches!(
            sender(&url).send(&email(), Uuid::now_v7()).await,
            Err(SendError::Retryable(_))
        ));
    }

    #[test]
    fn debug_output_hides_the_key() {
        let shown = format!("{:?}", sender(RESEND_API_URL));
        assert!(!shown.contains("re_test_key"), "{shown}");
        assert!(shown.contains("https://api.resend.com/emails"), "{shown}");
    }
}
