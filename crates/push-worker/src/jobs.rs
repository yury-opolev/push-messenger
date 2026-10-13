//! What the worker does with each kind of job.

use crate::accounts::{AccountEvent, Accounts};
use crate::queue::{Job, Outcome};

/// Runs jobs by kind: auth-svc's account events go to message-svc.
#[derive(Debug, Clone)]
pub struct Jobs {
    accounts: Accounts,
}

impl Jobs {
    pub fn new(accounts: Accounts) -> Self {
        Self { accounts }
    }

    /// One attempt at `job`. A kind nobody handles, or a payload that does not fit its
    /// kind, is poison.
    pub async fn run(&self, job: &Job) -> Outcome {
        match AccountEvent::parse(&job.kind, &job.payload) {
            Some(Ok(event)) => self.accounts.apply(event).await,
            Some(Err(reason)) => Outcome::Poison(reason),
            None => Outcome::Poison(format!("no handler for jobs of kind {:?}", job.kind)),
        }
    }
}
