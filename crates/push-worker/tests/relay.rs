//! The relay end to end (Docker must be running): auth-svc's outbox in its own database,
//! drained by the worker's role into the worker's queue, and applied in an in-process
//! message-svc through its `Accounts` API.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use push_auth::outbox::Event;
use push_domain::{InMemoryStore, Messaging, RecordingNotifier, SystemClock};
use push_email::{LogSender, Templates};
use push_message_svc::{AccountsApi, InternalToken, MessagingApi};
use push_proto::id_bytes;
use push_proto::internal::v1::IdentifyRequest;
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::MessagingServer;
use push_worker::accounts::Accounts;
use push_worker::email::Mailer;
use push_worker::jobs::Jobs;
use push_worker::queue::{Job, Outcome, Queue};
use push_worker::relay::OutboxRelay;
use serde_json::json;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Channel;
use uuid::Uuid;

/// The shared token message-svc requires for its `Accounts` API.
const ACCOUNTS_TOKEN: &str = "accounts-token-for-the-relay-tests";

struct World {
    _container: ContainerAsync<Postgres>,
    /// auth-svc's database as its owner, to write events.
    auth: PgPool,
    queue: Queue,
    relay: OutboxRelay,
    mailer: Mailer,
    jobs: Jobs,
    message_svc: String,
    messaging: MessagingClient<Channel>,
}

/// Postgres with auth-svc's database (migrated, the worker's role granted its access) and
/// the worker's (owned by the worker's role, migrated), and message-svc in this process.
async fn world() -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let server = format!("127.0.0.1:{port}");
    let admin = PgPool::connect(&format!("postgres://postgres:postgres@{server}/postgres"))
        .await
        .unwrap();
    for statement in [
        "CREATE ROLE push_worker LOGIN PASSWORD 'worker-secret'",
        "CREATE DATABASE push_auth",
        "CREATE DATABASE push_worker OWNER push_worker",
    ] {
        sqlx::query(statement).execute(&admin).await.unwrap();
    }
    admin.close().await;
    let auth_url = format!("postgres://postgres:postgres@{server}/push_auth");
    push_auth::migrate_database(&auth_url).await.unwrap();
    push_auth::grant_worker_access(&auth_url, "push_worker")
        .await
        .unwrap();
    let worker_url = format!("postgres://push_worker:worker-secret@{server}/push_worker");
    push_worker::migrate_database(&worker_url).await.unwrap();

    let pool = PgPool::connect(&worker_url).await.unwrap();
    let queue = Queue::new(pool.clone(), Duration::from_secs(60));
    let mailer = Mailer::new(pool, Arc::new(LogSender), Templates::default());
    let source = PgPool::connect(&format!(
        "postgres://push_worker:worker-secret@{server}/push_auth"
    ))
    .await
    .unwrap();
    let message_svc = message_svc().await;
    World {
        _container: container,
        auth: PgPool::connect(&auth_url).await.unwrap(),
        relay: OutboxRelay::new(source, queue.clone(), 100),
        queue,
        jobs: Jobs::new(
            Accounts::new(&message_svc, ACCOUNTS_TOKEN).unwrap(),
            mailer.clone(),
        ),
        mailer,
        messaging: MessagingClient::connect(message_svc.clone()).await.unwrap(),
        message_svc,
    }
}

/// message-svc's `Messaging` and `Accounts` APIs on an in-memory store; its URL.
async fn message_svc() -> String {
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        Arc::new(RecordingNotifier::default()),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tonic::transport::Server::builder()
        .add_service(MessagingServer::new(MessagingApi::new(Arc::clone(
            &messaging,
        ))))
        .add_service(AccountsServer::with_interceptor(
            AccountsApi::new(messaging),
            InternalToken::accounts(Some(ACCOUNTS_TOKEN.to_owned())),
        ))
        .serve_with_incoming(TcpListenerStream::new(listener));
    tokio::spawn(server);
    url
}

/// Writes `event` to auth-svc's outbox as auth-svc does; returns its id.
async fn write(pool: &PgPool, event: &Event) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(event.kind())
        .bind(event.payload())
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn outbox_size(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM auth.outbox")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Runs every due job once, in the order given; the outcomes.
async fn run(world: &World, jobs: &[Job]) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for job in jobs {
        let outcome = world.jobs.run(job).await;
        world.queue.finish(job, outcome.clone()).await.unwrap();
        outcomes.push(outcome);
    }
    outcomes
}

async fn identifies(world: &World, user: Uuid, device: Uuid) -> bool {
    world
        .messaging
        .clone()
        .identify(IdentifyRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
        })
        .await
        .is_ok()
}

#[tokio::test]
async fn events_move_from_the_outbox_to_the_queue_once() {
    let world = world().await;
    let user = Uuid::now_v7();
    let first = write(&world.auth, &Event::UserVerified { user_id: user }).await;
    let second = write(&world.auth, &Event::UserPasswordReset { user_id: user }).await;
    // A worker that died after queueing the first event, before deleting it.
    world
        .queue
        .enqueue(&[push_worker::queue::NewJob {
            id: first,
            kind: "user.verified".to_owned(),
            payload: json!({"user_id": user}),
        }])
        .await
        .unwrap();

    assert_eq!(world.relay.drain_once().await.unwrap(), 2);
    assert_eq!(outbox_size(&world.auth).await, 0);
    assert_eq!(world.relay.drain_once().await.unwrap(), 0);
    let ids: Vec<Uuid> = world
        .queue
        .claim(10)
        .await
        .unwrap()
        .iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(ids, [first, second]);
}

#[tokio::test]
async fn relayed_events_register_and_revoke_devices_in_message_svc_in_any_order() {
    let world = world().await;
    let (user, phone, tablet) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for event in [
        Event::UserVerified { user_id: user },
        Event::DeviceRegistered {
            user_id: user,
            device_id: phone,
        },
        Event::DeviceRegistered {
            user_id: user,
            device_id: tablet,
        },
        Event::DeviceRevoked {
            user_id: user,
            device_id: tablet,
        },
        Event::UserPasswordReset { user_id: user },
    ] {
        write(&world.auth, &event).await;
    }
    world.relay.drain_once().await.unwrap();
    let mut jobs = world.queue.claim(10).await.unwrap();
    // The worst order: everything backwards, so the revocation comes first.
    jobs.reverse();
    let outcomes = run(&world, &jobs).await;
    assert_eq!(outcomes, vec![Outcome::Done; 5]);
    assert!(identifies(&world, user, phone).await);
    assert!(!identifies(&world, user, tablet).await);

    // Delivered again (at least once): nothing changes.
    for event in [
        Event::DeviceRegistered {
            user_id: user,
            device_id: tablet,
        },
        Event::DeviceRegistered {
            user_id: user,
            device_id: phone,
        },
    ] {
        write(&world.auth, &event).await;
    }
    world.relay.drain_once().await.unwrap();
    let jobs = world.queue.claim(10).await.unwrap();
    assert_eq!(run(&world, &jobs).await, vec![Outcome::Done; 2]);
    assert!(identifies(&world, user, phone).await);
    assert!(!identifies(&world, user, tablet).await);
}

#[tokio::test]
async fn events_that_can_never_apply_are_poison_and_outages_are_retried() {
    let world = world().await;
    let (alice, bob, device) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    write(
        &world.auth,
        &Event::DeviceRegistered {
            user_id: alice,
            device_id: device,
        },
    )
    .await;
    world.relay.drain_once().await.unwrap();
    let jobs = world.queue.claim(10).await.unwrap();
    assert_eq!(run(&world, &jobs).await, [Outcome::Done]);

    // Bob's event names Alice's device; another event is malformed; a kind nobody knows.
    write(
        &world.auth,
        &Event::DeviceRevoked {
            user_id: bob,
            device_id: device,
        },
    )
    .await;
    world.relay.drain_once().await.unwrap();
    let mut jobs = world.queue.claim(10).await.unwrap();
    jobs.push(Job {
        id: Uuid::now_v7(),
        kind: "device.registered".to_owned(),
        payload: json!({"user_id": bob}),
        attempts: 1,
        claimed_at: Utc::now(),
    });
    jobs.push(Job {
        id: Uuid::now_v7(),
        kind: "sms.send".to_owned(),
        payload: json!({}),
        attempts: 1,
        claimed_at: Utc::now(),
    });
    for job in &jobs {
        let outcome = world.jobs.run(job).await;
        assert!(
            matches!(outcome, Outcome::Poison(_)),
            "{}: {outcome:?}",
            job.kind
        );
    }
    assert!(identifies(&world, alice, device).await);

    // message-svc is not there, or refuses a wrong token: retried, for as long as it
    // takes.
    let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gone = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    let wrong_token = "not-the-accounts-token-of-this-cluster";
    for accounts in [
        Accounts::new(&gone, ACCOUNTS_TOKEN).unwrap(),
        Accounts::new(&world.message_svc, wrong_token).unwrap(),
    ] {
        let outcome = Jobs::new(accounts, world.mailer.clone())
            .run(&Job {
                id: Uuid::now_v7(),
                kind: "user.verified".to_owned(),
                payload: json!({"user_id": alice}),
                attempts: 1,
                claimed_at: Utc::now(),
            })
            .await;
        assert!(
            matches!(
                outcome,
                Outcome::Failed {
                    max_attempts: None,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }
}
