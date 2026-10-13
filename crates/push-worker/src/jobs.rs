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
    mailer: Mailer,
    cleanup: Option<Cleanup>,
}

impl Jobs {
    pub fn new(accounts: Accounts, mailer: Mailer) -> Self {
        Self {
            accounts,
            mailer,
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
            return self.mailer.send(job).await;
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
