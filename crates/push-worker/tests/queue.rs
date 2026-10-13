//! The job queue on a real PostgreSQL (Docker must be running): the migration as a plain
//! owner, idempotent enqueueing, claims that never hand one job to two workers, leases
//! that expire, stale outcomes that never overwrite a newer claim, jobs given back at
//! shutdown, backoff, burial without links and cleanup.

use std::collections::HashSet;
use std::time::Duration;

use push_worker::queue::{NewJob, Outcome, Queue};
use push_worker::{MIGRATOR, migrate_database};
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

/// A running Postgres with a database `push_worker` owned by the plain role `push_worker`,
/// migrated as that role, and a pool on it.
async fn worker_database() -> (ContainerAsync<Postgres>, PgPool) {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let admin = PgPool::connect(&format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
    ))
    .await
    .expect("connect as the superuser");
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement)
            .execute(&admin)
            .await
            .expect(statement);
    }
    admin.close().await;
    let url = format!("postgres://push_worker:worker-secret@127.0.0.1:{port}/push_worker");
    migrate_database(&url).await.expect("migrate as the owner");
    migrate_database(&url).await.expect("a second run");
    let pool = PgPool::connect(&url).await.expect("connect");
    (container, pool)
}

fn job(kind: &str) -> NewJob {
    NewJob {
        id: Uuid::now_v7(),
        kind: kind.to_owned(),
        payload: json!({"n": 1}),
    }
}

async fn job_row(pool: &PgPool, id: Uuid) -> Option<(i32, Option<String>, bool, bool)> {
    sqlx::query_as(
        "SELECT attempts, failed_reason, dead_at IS NOT NULL, run_after > now() \
         FROM worker.job WHERE job_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_owner_migrates_and_a_job_is_added_once_claimed_once_and_deleted() {
    let (_container, pool) = worker_database().await;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(applied, MIGRATOR.iter().count() as i64);

    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    let first = job("user.verified");
    assert_eq!(
        queue
            .enqueue(&[first.clone(), job("email.send")])
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        queue.enqueue(std::slice::from_ref(&first)).await.unwrap(),
        0
    );

    let claimed = queue.claim(10).await.unwrap();
    assert_eq!(claimed.len(), 2);
    let job = &claimed[0];
    assert_eq!(
        (job.id, job.kind.as_str(), &job.payload, job.attempts),
        (first.id, "user.verified", &json!({"n": 1}), 1)
    );
    assert!(
        queue.claim(10).await.unwrap().is_empty(),
        "leased jobs are not due"
    );
    assert!(queue.finish(job, Outcome::Done).await.unwrap());
    assert_eq!(job_row(&pool, first.id).await, None);
}

#[tokio::test]
async fn concurrent_claims_never_share_a_job() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool, Duration::from_secs(60));
    let jobs: Vec<NewJob> = (0..60).map(|_| job("email.send")).collect();
    queue.enqueue(&jobs).await.unwrap();

    let claims: Vec<_> = (0..6)
        .map(|_| {
            let queue = queue.clone();
            tokio::spawn(async move { queue.claim(15).await.unwrap() })
        })
        .collect();
    let mut seen = HashSet::new();
    for claim in claims {
        for job in claim.await.unwrap() {
            assert!(seen.insert(job.id), "a job was claimed twice");
        }
    }
    assert_eq!(seen.len(), 60);
}

#[tokio::test]
async fn a_job_held_by_a_crashed_worker_comes_back_when_its_lease_ends() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool, Duration::from_millis(300));
    queue.enqueue(&[job("device.registered")]).await.unwrap();
    let crashed = queue.claim(1).await.unwrap();
    assert_eq!(crashed[0].attempts, 1);
    assert!(queue.claim(1).await.unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let again = queue.claim(1).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!((again[0].id, again[0].attempts), (crashed[0].id, 2));
}

#[tokio::test]
async fn a_stale_outcome_never_overwrites_a_newer_claim() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool.clone(), Duration::from_millis(300));
    let periodic = job("cleanup");
    queue
        .enqueue(&[job("email.send"), periodic.clone()])
        .await
        .unwrap();
    let slow = queue.claim(10).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The newer claim takes a long lease, so the checks below never race its expiry.
    let newer = Queue::new(pool.clone(), Duration::from_secs(60))
        .claim(10)
        .await
        .unwrap();
    assert_eq!(newer.len(), 2);

    // The slow worker's outcomes come after its lease ended: all of them are dropped.
    for (job, outcome) in [
        (&slow[0], Outcome::Done),
        (&slow[0], Outcome::Poison("too late".to_owned())),
        (
            &slow[1],
            Outcome::Failed {
                reason: "too late".to_owned(),
                max_attempts: None,
            },
        ),
        (&slow[1], Outcome::RunAgain(Duration::from_secs(3600))),
    ] {
        assert!(!queue.finish(job, outcome).await.unwrap());
    }
    for job in &newer {
        assert_eq!(
            job_row(&pool, job.id).await,
            Some((2, None, false, true)),
            "the newer claim is untouched"
        );
    }

    // A periodic job's attempts start again at 0 after each run; the claim time still
    // tells a later claim from the one that ran it.
    let periodic_claim = newer.iter().find(|job| job.id == periodic.id).unwrap();
    assert!(
        queue
            .finish(periodic_claim, Outcome::RunAgain(Duration::ZERO))
            .await
            .unwrap()
    );
    let next = queue.claim(10).await.unwrap();
    assert_eq!((next[0].id, next[0].attempts), (periodic.id, 1));
    let stale = slow.iter().find(|job| job.id == periodic.id).unwrap();
    assert_eq!(stale.attempts, 1);
    assert!(!queue.finish(stale, Outcome::Done).await.unwrap());
    assert!(queue.finish(&next[0], Outcome::Done).await.unwrap());
}

#[tokio::test]
async fn jobs_given_back_at_shutdown_are_due_at_once_without_the_attempt() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    queue
        .enqueue(&[job("email.send"), job("device.revoked")])
        .await
        .unwrap();
    let claimed = queue.claim(10).await.unwrap();
    assert_eq!(queue.release(&claimed).await.unwrap(), 2);
    assert_eq!(
        queue.release(&claimed).await.unwrap(),
        0,
        "no longer under that claim"
    );
    for job in &claimed {
        assert_eq!(job_row(&pool, job.id).await, Some((0, None, false, false)));
    }
    let again = queue.claim(10).await.unwrap();
    assert_eq!(again.len(), 2);
    assert!(again.iter().all(|job| job.attempts == 1));
    assert!(!queue.finish(&claimed[0], Outcome::Done).await.unwrap());
}

#[tokio::test]
async fn failures_back_off_poison_is_buried_and_periodic_jobs_run_again() {
    let (_container, pool) = worker_database().await;
    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    let limited = NewJob {
        payload: json!({
            "template": "verify_email",
            "params": {"link": "https://push.test/v1/auth/verify?token=secret", "handle": "alice"},
        }),
        ..job("email.send")
    };
    let (flaky, poison, periodic) = (job("email.send"), job("device.revoked"), job("cleanup"));
    queue
        .enqueue(&[
            flaky.clone(),
            limited.clone(),
            poison.clone(),
            periodic.clone(),
        ])
        .await
        .unwrap();
    let claimed = queue.claim(10).await.unwrap();
    let find = |id: Uuid| claimed.iter().find(|job| job.id == id).unwrap();

    let failed = |max_attempts| Outcome::Failed {
        reason: "Resend answered 503".to_owned(),
        max_attempts,
    };
    queue.finish(find(flaky.id), failed(None)).await.unwrap();
    queue
        .finish(find(limited.id), failed(Some(1)))
        .await
        .unwrap();
    queue
        .finish(find(poison.id), Outcome::Poison("bad payload".to_owned()))
        .await
        .unwrap();
    queue
        .finish(
            find(periodic.id),
            Outcome::RunAgain(Duration::from_secs(3600)),
        )
        .await
        .unwrap();

    assert_eq!(
        job_row(&pool, flaky.id).await,
        Some((1, Some("Resend answered 503".to_owned()), false, true))
    );
    assert_eq!(
        job_row(&pool, limited.id).await,
        Some((1, Some("Resend answered 503".to_owned()), true, true))
    );
    assert_eq!(
        job_row(&pool, poison.id).await,
        Some((1, Some("bad payload".to_owned()), true, true))
    );
    assert_eq!(
        job_row(&pool, periodic.id).await,
        Some((0, None, false, true))
    );
    // A buried email keeps its payload for inspection, but not the link.
    let kept: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM worker.job WHERE job_id = $1")
            .bind(limited.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        kept,
        json!({"template": "verify_email", "params": {"handle": "alice"}})
    );

    // The backoff of the first failure is 1 s; the buried jobs never come back.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let due: Vec<Uuid> = queue
        .claim(10)
        .await
        .unwrap()
        .iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(due, [flaky.id]);

    assert_eq!(
        queue
            .delete_dead(Duration::from_secs(3600), 10)
            .await
            .unwrap(),
        0
    );
    assert_eq!(queue.delete_dead(Duration::ZERO, 10).await.unwrap(), 2);
    assert_eq!(job_row(&pool, poison.id).await, None);
}
