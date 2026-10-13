//! The outbox relay (spec §2): auth-svc's outbox, moved into the job queue.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::queue::{NewJob, Queue};

/// Moves events from `auth.outbox` into the worker's queue, at least once and never lost.
///
/// One pass locks a batch of the oldest events (`FOR UPDATE SKIP LOCKED`, so several
/// workers drain side by side), adds them to the queue, which commits in the worker's own
/// database, and only then deletes them from the outbox and commits there. A worker that
/// dies in between leaves the events in the outbox; the next pass adds them again, which
/// the queue ignores, since a job's id is its event's id.
#[derive(Debug, Clone)]
pub struct OutboxRelay {
    source: PgPool,
    queue: Queue,
    batch: i64,
}

impl OutboxRelay {
    /// `source` is a pool on auth-svc's database as the worker's role, which may read and
    /// delete `auth.outbox` and nothing else; `batch` events move per pass.
    pub fn new(source: PgPool, queue: Queue, batch: i64) -> Self {
        Self {
            source,
            queue,
            batch,
        }
    }

    /// Moves one batch; returns how many events it moved (0 when the outbox is empty).
    pub async fn drain_once(&self) -> sqlx::Result<usize> {
        let mut tx = self.source.begin().await?;
        let events: Vec<(Uuid, String, Value)> = sqlx::query_as(
            "SELECT event_id, kind, payload FROM auth.outbox \
             ORDER BY created_at, event_id LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind(self.batch)
        .fetch_all(&mut *tx)
        .await?;
        if events.is_empty() {
            return Ok(0);
        }
        let ids: Vec<Uuid> = events.iter().map(|(id, _, _)| *id).collect();
        let jobs: Vec<NewJob> = events
            .into_iter()
            .map(|(id, kind, payload)| NewJob { id, kind, payload })
            .collect();
        self.queue.enqueue(&jobs).await?;
        sqlx::query("DELETE FROM auth.outbox WHERE event_id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(ids.len())
    }
}
