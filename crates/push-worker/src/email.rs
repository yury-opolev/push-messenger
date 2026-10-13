//! Sending email (spec §9): `email.send` jobs are rendered, checked against the
//! suppression list, recorded in `worker.email_message` and handed to the sender.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use push_email::{EmailSender, OutgoingEmail, SendError, Templates, recipient_domain};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::queue::{Job, Outcome};

/// An email is given up after this many attempts, about half an hour with the backoff of
/// the queue: by then its link has expired anyway.
pub const EMAIL_MAX_ATTEMPTS: i32 = 12;

const QUEUED: i16 = 1;
const SENT: i16 = 2;
const SUPPRESSED: i16 = 3;
const FAILED: i16 = 4;

/// Sends the emails of `email.send` jobs.
#[derive(Clone)]
pub struct Mailer {
    pool: PgPool,
    sender: Arc<dyn EmailSender>,
    templates: Templates,
}

impl std::fmt::Debug for Mailer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Mailer")
            .field("templates", &self.templates)
            .finish_non_exhaustive()
    }
}

impl Mailer {
    pub fn new(pool: PgPool, sender: Arc<dyn EmailSender>, templates: Templates) -> Self {
        Self {
            pool,
            sender,
            templates,
        }
    }

    /// One attempt at an `email.send` job (`{user_id, to, template, params}`). The job's id
    /// is the email's id and the provider's idempotency key, so a retry never sends a
    /// second copy, and an email already sent, suppressed or failed is not tried again.
    pub async fn send(&self, job: &Job) -> Outcome {
        match self.attempt(job).await {
            Ok(outcome) => outcome,
            Err(_) => Outcome::Failed {
                reason: "the worker's database did not answer".to_owned(),
                max_attempts: None,
            },
        }
    }

    async fn attempt(&self, job: &Job) -> sqlx::Result<Outcome> {
        let payload = &job.payload;
        let (Some(to), Some(template)) = (payload["to"].as_str(), payload["template"].as_str())
        else {
            return Ok(Outcome::Poison(
                "email.send: the payload has no to or template".to_owned(),
            ));
        };
        let params = &payload["params"];
        let status: Option<i16> =
            sqlx::query_scalar("SELECT status FROM worker.email_message WHERE email_id = $1")
                .bind(job.id)
                .fetch_optional(&self.pool)
                .await?;
        if status.is_some_and(|status| status != QUEUED) {
            return Ok(Outcome::Done);
        }
        let suppressed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM worker.email_suppression WHERE address = $1::citext)",
        )
        .bind(to)
        .fetch_one(&self.pool)
        .await?;
        if suppressed {
            self.record(job.id, to, template, params, SUPPRESSED)
                .await?;
            tracing::info!(email_id = %job.id, template, "email not sent: the address is suppressed");
            return Ok(Outcome::Done);
        }
        let content = match self.templates.render(template, params) {
            Ok(content) => content,
            Err(error) => {
                self.record(job.id, to, template, params, FAILED).await?;
                return Ok(Outcome::Poison(error.to_string()));
            }
        };
        self.record(job.id, to, template, params, QUEUED).await?;
        let email = OutgoingEmail {
            to: to.to_owned(),
            template: template.to_owned(),
            content,
        };
        match self.sender.send(&email, job.id).await {
            Ok(provider_message_id) => {
                sqlx::query(
                    "UPDATE worker.email_message SET status = $2, provider_message_id = $3, \
                     sent_at = now() WHERE email_id = $1",
                )
                .bind(job.id)
                .bind(SENT)
                .bind(provider_message_id)
                .execute(&self.pool)
                .await?;
                Ok(Outcome::Done)
            }
            Err(SendError::Retryable(reason)) if job.attempts < EMAIL_MAX_ATTEMPTS => {
                Ok(Outcome::Failed {
                    reason,
                    max_attempts: None,
                })
            }
            Err(SendError::Retryable(reason) | SendError::Permanent(reason)) => {
                // Only while still queued: a late duplicate run must not undo a send.
                sqlx::query(
                    "UPDATE worker.email_message SET status = $2 WHERE email_id = $1 AND status = 1",
                )
                    .bind(job.id)
                    .bind(FAILED)
                    .execute(&self.pool)
                    .await?;
                Ok(Outcome::Poison(reason))
            }
        }
    }

    /// Records the email with `status`, or moves a queued one to `status`. Its link is
    /// left out: it would sign its holder in.
    async fn record(
        &self,
        id: Uuid,
        to: &str,
        template: &str,
        params: &Value,
        status: i16,
    ) -> sqlx::Result<()> {
        let mut kept = params.clone();
        if let Some(params) = kept.as_object_mut() {
            params.remove("link");
        }
        sqlx::query(
            "INSERT INTO worker.email_message \
             (email_id, to_address, recipient_domain, template, params, status) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (email_id) DO UPDATE SET status = EXCLUDED.status \
             WHERE worker.email_message.status = 1",
        )
        .bind(id)
        .bind(to)
        .bind(recipient_domain(to))
        .bind(template)
        .bind(kept)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// How many emails the DEV ONLY mailbox keeps, newest first.
const MAILBOX_SIZE: usize = 1000;
/// How many emails one look into the mailbox shows.
const MAILBOX_PAGE: usize = 20;

/// DEV ONLY: the emails the log sender delivered, in memory, so a local cluster and the
/// smoke test can open their links.
#[derive(Debug, Clone, Default)]
pub struct Mailbox {
    emails: Arc<Mutex<VecDeque<OutgoingEmail>>>,
}

impl Mailbox {
    /// The newest emails to `address` (any case), newest first, at most 20.
    pub fn to(&self, address: &str) -> Vec<OutgoingEmail> {
        let emails = self
            .emails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        emails
            .iter()
            .filter(|email| email.to.eq_ignore_ascii_case(address.trim()))
            .take(MAILBOX_PAGE)
            .cloned()
            .collect()
    }

    fn deliver(&self, email: &OutgoingEmail) {
        let mut emails = self
            .emails
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        emails.push_front(email.clone());
        emails.truncate(MAILBOX_SIZE);
    }
}

/// DEV ONLY: a sender that also drops every email it sent into a [`Mailbox`].
#[derive(Debug, Clone)]
pub struct MailboxSender<S> {
    inner: S,
    mailbox: Mailbox,
}

impl<S> MailboxSender<S> {
    pub fn new(inner: S, mailbox: Mailbox) -> Self {
        Self { inner, mailbox }
    }
}

#[async_trait]
impl<S: EmailSender> EmailSender for MailboxSender<S> {
    async fn send(
        &self,
        email: &OutgoingEmail,
        idempotency_key: Uuid,
    ) -> Result<String, SendError> {
        let id = self.inner.send(email, idempotency_key).await?;
        self.mailbox.deliver(email);
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use push_email::{LogSender, Rendered};

    use super::*;

    fn email(to: &str, subject: &str) -> OutgoingEmail {
        OutgoingEmail {
            to: to.to_owned(),
            template: "verify_email".to_owned(),
            content: Rendered {
                subject: subject.to_owned(),
                text: String::new(),
                html: String::new(),
            },
        }
    }

    #[tokio::test]
    async fn the_mailbox_shows_what_was_sent_to_an_address_newest_first() {
        let mailbox = Mailbox::default();
        let sender = MailboxSender::new(LogSender, mailbox.clone());
        for (to, subject) in [
            ("alice@example.com", "first"),
            ("bob@example.com", "other"),
            ("Alice@Example.com", "second"),
        ] {
            sender
                .send(&email(to, subject), Uuid::now_v7())
                .await
                .unwrap();
        }
        let subjects: Vec<String> = mailbox
            .to(" alice@example.com")
            .into_iter()
            .map(|email| email.content.subject)
            .collect();
        assert_eq!(subjects, ["second", "first"]);
        assert!(mailbox.to("carol@example.com").is_empty());
    }

    #[test]
    fn the_mailbox_keeps_the_newest_emails_only() {
        let mailbox = Mailbox::default();
        for n in 0..MAILBOX_SIZE + 5 {
            mailbox.deliver(&email("a@example.com", &n.to_string()));
        }
        assert_eq!(mailbox.emails.lock().unwrap().len(), MAILBOX_SIZE);
        let newest = mailbox.to("a@example.com");
        assert_eq!(newest.len(), MAILBOX_PAGE);
        assert_eq!(newest[0].content.subject, (MAILBOX_SIZE + 4).to_string());
    }
}
