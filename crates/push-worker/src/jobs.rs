//! What the worker does with each kind of job.

use crate::accounts::{AccountEvent, Accounts};
use crate::email::Mailer;
use crate::queue::{Job, Outcome};

/// Runs jobs by kind: auth-svc's account events go to message-svc, `email.send` to the
/// mailer.
#[derive(Debug, Clone)]
pub struct Jobs {
    accounts: Accounts,
    mailer: Mailer,
}

impl Jobs {
    pub fn new(accounts: Accounts, mailer: Mailer) -> Self {
        Self { accounts, mailer }
    }

    /// One attempt at `job`. A kind nobody handles, or a payload that does not fit its
    /// kind, is poison.
    pub async fn run(&self, job: &Job) -> Outcome {
        if job.kind == "email.send" {
            return self.mailer.send(job).await;
        }
        match AccountEvent::parse(&job.kind, &job.payload) {
            Some(Ok(event)) => self.accounts.apply(event).await,
            Some(Err(reason)) => Outcome::Poison(reason),
            None => Outcome::Poison(format!("no handler for jobs of kind {:?}", job.kind)),
        }
    }
}
