//! The provider-neutral sender (spec §9) and the log-only sender of local development.

use async_trait::async_trait;
use uuid::Uuid;

use crate::templates::Rendered;

/// One email to one address, rendered.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingEmail {
    pub to: String,
    /// The template it was rendered from; senders may tag the email with it.
    pub template: String,
    pub content: Rendered,
}

impl std::fmt::Debug for OutgoingEmail {
    /// The template and the recipient's domain only: no address, no body.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutgoingEmail")
            .field("template", &self.template)
            .field("recipient_domain", &recipient_domain(&self.to))
            .finish_non_exhaustive()
    }
}

/// Why sending failed. The message never contains the address or the email.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// Worth trying again later: a timeout, a rate limit, a provider outage.
    #[error("sending failed for now: {0}")]
    Retryable(String),
    /// Trying again cannot help: the provider refused this email.
    #[error("sending failed for good: {0}")]
    Permanent(String),
}

/// Sends email through a provider.
#[async_trait]
pub trait EmailSender: Send + Sync {
    /// Sends `email` and returns the provider's id for it. `idempotency_key` names this
    /// email: sending it again with the same key must not deliver a second copy.
    async fn send(&self, email: &OutgoingEmail, idempotency_key: Uuid)
    -> Result<String, SendError>;
}

/// The local sender: logs that an email went out, with its template and the recipient's
/// domain, and sends nothing (spec §9).
#[derive(Debug, Clone, Copy, Default)]
pub struct LogSender;

#[async_trait]
impl EmailSender for LogSender {
    async fn send(
        &self,
        email: &OutgoingEmail,
        idempotency_key: Uuid,
    ) -> Result<String, SendError> {
        tracing::info!(
            email_id = %idempotency_key,
            template = %email.template,
            recipient_domain = %recipient_domain(&email.to),
            "email delivered by the log sender (not sent)"
        );
        Ok(format!("log-{idempotency_key}"))
    }
}

/// The lower-cased domain of an address, for metrics and logs; empty without an `@`.
pub fn recipient_domain(address: &str) -> String {
    address
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email() -> OutgoingEmail {
        OutgoingEmail {
            to: "alice@Example.COM".to_owned(),
            template: "verify_email".to_owned(),
            content: Rendered {
                subject: "Verify".to_owned(),
                text: "https://x.test/?token=secret".to_owned(),
                html: "<a>secret</a>".to_owned(),
            },
        }
    }

    #[tokio::test]
    async fn the_log_sender_answers_with_an_id_of_its_own() {
        let key = Uuid::now_v7();
        assert_eq!(
            LogSender.send(&email(), key).await,
            Ok(format!("log-{key}"))
        );
    }

    #[test]
    fn debug_output_shows_neither_the_address_nor_the_body() {
        let shown = format!("{:?}", email());
        assert!(shown.contains("example.com"), "{shown}");
        assert!(!shown.contains("alice"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn the_recipient_domain_is_lower_case() {
        assert_eq!(recipient_domain("a@Example.COM"), "example.com");
        assert_eq!(recipient_domain("a@b@c.test"), "c.test");
        assert_eq!(recipient_domain("nobody"), "");
    }
}
