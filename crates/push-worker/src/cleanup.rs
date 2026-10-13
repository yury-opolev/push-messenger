//! The cleanup jobs: periodic jobs in the queue, so one replica runs each at a time.

use std::time::{Duration, Instant};

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::queue::{NewJob, Outcome, Queue};

/// Deletes expired refresh tokens in auth-svc's database.
pub const REFRESH_TOKENS: &str = "cleanup.refresh_tokens";
/// Deletes accounts that stayed unverified, in auth-svc's database.
pub const UNVERIFIED_ACCOUNTS: &str = "cleanup.unverified_accounts";
/// Deletes the worker's own old records: buried jobs, emails and delivery events.
pub const WORKER_RECORDS: &str = "cleanup.worker_records";

/// Rows deleted per statement.
const BATCH: i32 = 1000;
/// Batches per run; a run that leaves more behind comes back after [`AGAIN_SOON`].
const MAX_BATCHES: u32 = 50;
/// A run starts no batch after this long, and comes back after [`AGAIN_SOON`] too: with a
/// batch's statement timeout of 10 s, a run takes at most about 15 s, like any other job,
/// well inside the queue's lease and the pod's grace period.
const RUN_BUDGET: Duration = Duration::from_secs(5);
const AGAIN_SOON: Duration = Duration::from_secs(60);

/// How long things are kept, and how often the cleanups run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Accounts unverified for longer are deleted.
    pub unverified_accounts: Duration,
    /// Buried jobs, email records and delivery events older than this are deleted.
    pub worker_records: Duration,
    /// Each cleanup runs this often.
    pub interval: Duration,
}

/// The cleanup jobs, with fixed ids: adding them again on every start changes nothing.
pub fn periodic_jobs() -> Vec<NewJob> {
    [
        (0xc1ea_0001, REFRESH_TOKENS),
        (0xc1ea_0002, UNVERIFIED_ACCOUNTS),
        (0xc1ea_0003, WORKER_RECORDS),
    ]
    .into_iter()
    .map(|(id, kind)| NewJob {
        id: Uuid::from_u128(id),
        kind: kind.to_owned(),
        payload: json!({}),
    })
    .collect()
}

/// Runs the cleanups: auth-svc's through the two functions it lets the worker's role
/// call, the worker's own directly.
#[derive(Debug, Clone)]
pub struct Cleanup {
    auth: PgPool,
    queue: Queue,
    worker: PgPool,
    retention: Retention,
}

impl Cleanup {
    /// `auth` is the pool on auth-svc's database as the worker's role, `worker` the
    /// worker's own database.
    pub fn new(auth: PgPool, worker: PgPool, queue: Queue, retention: Retention) -> Self {
        Self {
            auth,
            queue,
            worker,
            retention,
        }
    }

    /// Runs the cleanup `kind`; `None` if `kind` is not a cleanup.
    pub async fn run(&self, kind: &str) -> Option<Outcome> {
        let deadline = Instant::now() + RUN_BUDGET;
        let result = match kind {
            REFRESH_TOKENS => {
                self.batches(deadline, || {
                    sqlx::query_scalar("SELECT auth.delete_expired_refresh_tokens($1)")
                        .bind(BATCH)
                        .fetch_one(&self.auth)
                })
                .await
            }
            UNVERIFIED_ACCOUNTS => {
                let older_than = self.retention.unverified_accounts.as_secs_f64();
                self.batches(deadline, || {
                    sqlx::query_scalar(
                        "SELECT auth.delete_unverified_accounts(make_interval(secs => $1), $2)",
                    )
                    .bind(older_than)
                    .bind(BATCH)
                    .fetch_one(&self.auth)
                })
                .await
            }
            WORKER_RECORDS => self.worker_records(deadline).await,
            _ => return None,
        };
        Some(match result {
            Ok((deleted, done)) => {
                if deleted > 0 {
                    tracing::info!(cleanup = kind, deleted, "cleaned up");
                }
                Outcome::RunAgain(if done {
                    self.retention.interval
                } else {
                    AGAIN_SOON
                })
            }
            Err(_) => Outcome::Failed {
                reason: format!("{kind}: the database did not answer"),
                max_attempts: None,
            },
        })
    }

    /// Calls `delete` until a batch comes back short, at most [`MAX_BATCHES`] times and
    /// not after `deadline`. Returns how many rows went, and whether nothing is left.
    async fn batches<F, Fut>(&self, deadline: Instant, delete: F) -> sqlx::Result<(i64, bool)>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = sqlx::Result<i32>>,
    {
        let mut deleted = 0;
        for _ in 0..MAX_BATCHES {
            if Instant::now() >= deadline {
                break;
            }
            let batch = delete().await?;
            deleted += i64::from(batch);
            if batch < BATCH {
                return Ok((deleted, true));
            }
        }
        Ok((deleted, false))
    }

    async fn worker_records(&self, deadline: Instant) -> sqlx::Result<(i64, bool)> {
        let older_than = self.retention.worker_records;
        let jobs = self
            .batches(deadline, || async {
                let deleted = self.queue.delete_dead(older_than, i64::from(BATCH)).await?;
                Ok(deleted as i32)
            })
            .await?;
        let mut total = jobs;
        for (table, column, key) in [
            ("email_message", "created_at", "email_id"),
            ("email_event", "received_at", "event_id"),
        ] {
            let statement = format!(
                "WITH gone AS (DELETE FROM worker.{table} WHERE {key} IN \
                 (SELECT {key} FROM worker.{table} \
                 WHERE {column} < now() - make_interval(secs => $1) LIMIT $2) RETURNING 1) \
                 SELECT count(*)::integer FROM gone"
            );
            let (deleted, done) = self
                .batches(deadline, || {
                    sqlx::query_scalar(&statement)
                        .bind(older_than.as_secs_f64())
                        .bind(BATCH)
                        .fetch_one(&self.worker)
                })
                .await?;
            total = (total.0 + deleted, total.1 && done);
        }
        Ok(total)
    }
}
