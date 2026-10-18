//! What the worker does with each kind of job.

use crate::accounts::{AccountEvent, Accounts};
use crate::cleanup::Cleanup;
use crate::email::Mailer;
use crate::queue::{Job, Outcome};

/// Runs jobs by kind: auth-svc's account events go to message-svc, `email.send` to the
/// mailer, `cleanup.*` to the cleanups.
#[derive(Debug, Clone)]
pub struct Jobs {
    accounts: Accounts,
    mailer: Option<Mailer>,
    cleanup: Option<Cleanup>,
}

impl Jobs {
    pub fn new(accounts: Accounts, mailer: Mailer) -> Self {
        Self {
            accounts,
            mailer: Some(mailer),
            cleanup: None,
        }
    }

    /// The jobs of a worker that sends no email (`PUSH_EMAIL_SENDER=none`).
    pub fn without_email(accounts: Accounts) -> Self {
        Self {
            accounts,
            mailer: None,
            cleanup: None,
        }
    }

    /// Runs the cleanup jobs too.
    pub fn with_cleanup(self, cleanup: Cleanup) -> Self {
        Self {
            cleanup: Some(cleanup),
            ..self
        }
    }

    /// One attempt at `job`. A kind nobody handles, or a payload that does not fit its
    /// kind, is poison.
    pub async fn run(&self, job: &Job) -> Outcome {
        if job.kind == "email.send" {
            return match &self.mailer {
                Some(mailer) => mailer.send(job).await,
                // Buried without its link, like any poison (queue.rs).
                None => Outcome::Poison(
                    "email.send: email is off (PUSH_EMAIL_SENDER=none); nothing sends it"
                        .to_owned(),
                ),
            };
        }
        if let Some(cleanup) = &self.cleanup {
            if let Some(outcome) = cleanup.run(&job.kind).await {
                return outcome;
            }
        }
        match AccountEvent::parse(&job.kind, &job.payload) {
            Some(Ok(event)) => self.accounts.apply(event).await,
            Some(Err(reason)) => Outcome::Poison(reason),
            None => Outcome::Poison(format!("no handler for jobs of kind {:?}", job.kind)),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn without_email_an_email_job_is_buried_unsent() {
        let accounts =
            Accounts::new("http://127.0.0.1:1", "accounts-token-of-the-jobs-tests").unwrap();
        let job = Job {
            id: Uuid::now_v7(),
            kind: "email.send".to_owned(),
            payload: json!({
                "to": "a@example.com",
                "template": "verify_email",
                "params": {"link": "https://push.example/v1/auth/verify?token=t"},
            }),
            attempts: 1,
            claimed_at: Utc::now(),
        };
        let outcome = Jobs::without_email(accounts).run(&job).await;
        assert!(
            matches!(&outcome, Outcome::Poison(reason) if reason.contains("PUSH_EMAIL_SENDER=none")),
            "{outcome:?}"
        );
    }
}
