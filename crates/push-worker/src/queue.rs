//! The job queue in `worker.job` (spec §3.3): at least once, shared by every replica.
//!
//! A claim takes due jobs with `FOR UPDATE SKIP LOCKED`, so replicas never wait for each
//! other, and leases them: their `run_after` moves to the end of the lease and `attempts`
//! goes up. A job is deleted when it is done, put back with a backoff when it failed, and
//! buried (`dead_at`) when it can never succeed. A worker that dies holding a job only
//! delays it: the job is due again when the lease ends.
//!
//! A claim is named by the job's `attempts` and `claimed_at`, and every outcome is recorded
//! only while the job is still under that claim: when a slow worker's lease ended and
//! another worker claimed the job again, the slow worker's outcome is dropped instead of
//! overwriting the newer claim.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// The first retry waits this long; each further one twice as long as the one before.
pub const FIRST_RETRY: Duration = Duration::from_secs(1);
/// The longest wait between two attempts.
pub const MAX_RETRY: Duration = Duration::from_secs(10 * 60);
/// A reason is cut to this many characters.
const MAX_REASON_CHARS: usize = 500;

/// A job to add.
#[derive(Debug, Clone, PartialEq)]
pub struct NewJob {
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
}

/// A claimed job.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
    /// Attempts so far, this one included.
    pub attempts: i32,
    /// When this claim was made; with `attempts`, it names the claim.
    pub claimed_at: DateTime<Utc>,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Done: the job is deleted.
    Done,
    /// Done for now: a periodic job, due again after this long.
    RunAgain(Duration),
    /// Failed, worth trying again after a [`backoff`]; buried instead once the job has
    /// had `max_attempts` attempts (never, without a maximum).
    Failed {
        reason: String,
        max_attempts: Option<i32>,
    },
    /// Can never succeed (poison): buried at once.
    Poison(String),
}

/// The wait before the next attempt of a job that failed `attempts` times: 1 s, doubling,
/// at most [`MAX_RETRY`].
pub fn backoff(attempts: i32) -> Duration {
    let doublings = attempts.saturating_sub(1).clamp(0, 20) as u32;
    FIRST_RETRY.saturating_mul(1 << doublings).min(MAX_RETRY)
}

/// The queue on the worker's database.
#[derive(Debug, Clone)]
pub struct Queue {
    pool: PgPool,
    lease: Duration,
}

impl Queue {
    /// A claimed job is someone else's for `lease`; make it longer than any attempt takes.
    pub fn new(pool: PgPool, lease: Duration) -> Self {
        Self { pool, lease }
    }

    /// Adds `jobs`, due now. A job whose id is in the queue already is ignored, so adding
    /// the same outbox event twice is harmless. Returns how many were new.
    pub async fn enqueue(&self, jobs: &[NewJob]) -> sqlx::Result<u64> {
        let ids: Vec<Uuid> = jobs.iter().map(|job| job.id).collect();
        let kinds: Vec<&str> = jobs.iter().map(|job| job.kind.as_str()).collect();
        let payloads: Vec<Value> = jobs.iter().map(|job| job.payload.clone()).collect();
        let added = sqlx::query(
            "INSERT INTO worker.job (job_id, kind, payload) \
             SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::jsonb[]) \
             ON CONFLICT (job_id) DO NOTHING",
        )
        .bind(&ids)
        .bind(&kinds)
        .bind(&payloads)
        .execute(&self.pool)
        .await?;
        Ok(added.rows_affected())
    }

    /// Claims up to `limit` due jobs, oldest first, and leases them. Jobs another worker
    /// holds are skipped, not waited for.
    pub async fn claim(&self, limit: i64) -> sqlx::Result<Vec<Job>> {
        let rows: Vec<(Uuid, String, Value, i32, DateTime<Utc>)> = sqlx::query_as(
            "UPDATE worker.job SET attempts = attempts + 1, claimed_at = now(), \
             run_after = now() + make_interval(secs => $2) \
             WHERE job_id IN (SELECT job_id FROM worker.job \
                 WHERE dead_at IS NULL AND run_after <= now() \
                 ORDER BY run_after, job_id LIMIT $1 FOR UPDATE SKIP LOCKED) \
             RETURNING job_id, kind, payload, attempts, claimed_at",
        )
        .bind(limit)
        .bind(self.lease.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        let mut jobs: Vec<Job> = rows
            .into_iter()
            .map(|(id, kind, payload, attempts, claimed_at)| Job {
                id,
                kind,
                payload,
                attempts,
                claimed_at,
            })
            .collect();
        // UUIDv7 ids are time-ordered: handle older events first.
        jobs.sort_by_key(|job| job.id);
        Ok(jobs)
    }

    /// Records how the attempt at `job` ended, if the job is still under this claim, and
    /// returns whether it was. `false` means the lease ended and another worker claimed the
    /// job again: the outcome is dropped, never written over the newer claim.
    ///
    /// A buried job keeps its payload for inspection, without `params.link`: an email's
    /// link would sign its holder in.
    pub async fn finish(&self, job: &Job, outcome: Outcome) -> sqlx::Result<bool> {
        let under_claim = |sql: &'static str| {
            sqlx::query(sql)
                .bind(job.id)
                .bind(job.attempts)
                .bind(job.claimed_at)
        };
        let query = match outcome {
            Outcome::Done => under_claim(
                "DELETE FROM worker.job WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            ),
            Outcome::RunAgain(after) => under_claim(
                "UPDATE worker.job SET run_after = now() + make_interval(secs => $4), \
                 attempts = 0, claimed_at = NULL, failed_reason = NULL \
                 WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            )
            .bind(after.as_secs_f64()),
            Outcome::Failed {
                reason,
                max_attempts,
            } if max_attempts.is_none_or(|max| job.attempts < max) => under_claim(
                "UPDATE worker.job SET run_after = now() + make_interval(secs => $4), \
                 claimed_at = NULL, failed_reason = $5 \
                 WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            )
            .bind(backoff(job.attempts).as_secs_f64())
            .bind(cut(reason)),
            Outcome::Failed { reason, .. } | Outcome::Poison(reason) => under_claim(
                "UPDATE worker.job SET dead_at = now(), claimed_at = NULL, failed_reason = $4, \
                 payload = payload #- '{params,link}' \
                 WHERE job_id = $1 AND attempts = $2 AND claimed_at = $3",
            )
            .bind(cut(reason)),
        };
        Ok(query.execute(&self.pool).await?.rows_affected() == 1)
    }

    /// Gives back jobs claimed but not started (at shutdown): due again at once, the
    /// claim's attempt undone. A job no longer under that claim is left alone. Returns how
    /// many went back.
    pub async fn release(&self, jobs: &[Job]) -> sqlx::Result<u64> {
        let ids: Vec<Uuid> = jobs.iter().map(|job| job.id).collect();
        let attempts: Vec<i32> = jobs.iter().map(|job| job.attempts).collect();
        let claimed: Vec<DateTime<Utc>> = jobs.iter().map(|job| job.claimed_at).collect();
        let released = sqlx::query(
            "UPDATE worker.job AS job SET run_after = now(), attempts = job.attempts - 1, \
             claimed_at = NULL \
             FROM UNNEST($1::uuid[], $2::int4[], $3::timestamptz[]) \
                 AS claim (job_id, attempts, claimed_at) \
             WHERE job.job_id = claim.job_id AND job.attempts = claim.attempts \
                 AND job.claimed_at = claim.claimed_at",
        )
        .bind(&ids)
        .bind(&attempts)
        .bind(&claimed)
        .execute(&self.pool)
        .await?;
        Ok(released.rows_affected())
    }

    /// Deletes up to `batch` buried jobs that died more than `older_than` ago. Returns how
    /// many it deleted.
    pub async fn delete_dead(&self, older_than: Duration, batch: i64) -> sqlx::Result<u64> {
        let deleted = sqlx::query(
            "DELETE FROM worker.job WHERE job_id IN (SELECT job_id FROM worker.job \
             WHERE dead_at < now() - make_interval(secs => $1) LIMIT $2)",
        )
        .bind(older_than.as_secs_f64())
        .bind(batch)
        .execute(&self.pool)
        .await?;
        Ok(deleted.rows_affected())
    }
}

fn cut(reason: String) -> String {
    match reason.char_indices().nth(MAX_REASON_CHARS) {
        Some((end, _)) => reason[..end].to_owned(),
        None => reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_from_1_s_to_10_minutes() {
        let waits: Vec<u64> = (1..=12)
            .map(|attempts| backoff(attempts).as_secs())
            .collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 600, 600]);
        assert_eq!(backoff(0), FIRST_RETRY);
        assert_eq!(backoff(i32::MAX), MAX_RETRY);
    }

    #[test]
    fn long_reasons_are_cut() {
        assert_eq!(cut("short".to_owned()), "short");
        assert_eq!(cut("é".repeat(600)).chars().count(), MAX_REASON_CHARS);
    }
}
