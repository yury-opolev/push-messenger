//! The call engine's Redis scripts against a real Redis (Docker must be running).

use std::time::Duration;

use push_domain::{DeviceId, UserId};
use push_message_svc::calls::{Applied, DUE_KEY, RedisCalls, Write, call_key, user_key};
use push_proto::presence_conns_key;
use redis::aio::MultiplexedConnection;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

async fn redis() -> (ContainerAsync<Redis>, RedisCalls, MultiplexedConnection) {
    let container = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start a Redis container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("the Redis port");
    let url = format!("redis://127.0.0.1:{port}");
    let calls = RedisCalls::connect(&url).await.expect("connect");
    let admin = redis::Client::open(url.as_str())
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    (container, calls, admin)
}

fn create(call: Uuid, record: &str, due_ms: i64, ttl_ms: i64, lock: Vec<UserId>) -> Write<'_> {
    Write {
        call,
        expected: None,
        record: Some(record),
        ttl_ms,
        due_ms,
        lock,
        release: Vec::new(),
    }
}

async fn due_score(admin: &mut MultiplexedConnection, call: Uuid) -> Option<f64> {
    redis::cmd("ZSCORE")
        .arg(DUE_KEY)
        .arg(call.to_string())
        .query_async(admin)
        .await
        .unwrap()
}

async fn exists(admin: &mut MultiplexedConnection, key: &str) -> bool {
    let found: i64 = redis::cmd("EXISTS")
        .arg(key)
        .query_async(admin)
        .await
        .unwrap();
    found == 1
}

#[tokio::test]
async fn create_locks_both_users_and_a_second_create_is_busy_or_in_call() {
    let (_container, calls, mut admin) = redis().await;
    let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
    let call = Uuid::now_v7();
    let now = calls.now().await.unwrap();
    let write = create(call, "{\"a\":1}", now + 45_000, 105_000, vec![alice, bob]);
    assert_eq!(calls.apply(&write).await.unwrap(), Applied::Ok);
    assert_eq!(calls.user_call(alice).await.unwrap(), Some(call));
    assert_eq!(calls.user_call(bob).await.unwrap(), Some(call));
    assert_eq!(calls.user_call(carol).await.unwrap(), None);
    let (stored, loaded_at) = calls.load(call).await.unwrap();
    assert_eq!(stored, Some((1, "{\"a\":1}".to_owned())));
    assert!(loaded_at >= now);
    assert_eq!(
        due_score(&mut admin, call).await,
        Some((now + 45_000) as f64)
    );
    assert_eq!(calls.live_count().await.unwrap(), 1);

    let other = Uuid::now_v7();
    let caller_busy = create(other, "{}", now, 1_000, vec![alice, carol]);
    assert_eq!(calls.apply(&caller_busy).await.unwrap(), Applied::InCall);
    let callee_busy = create(other, "{}", now, 1_000, vec![carol, bob]);
    assert_eq!(calls.apply(&callee_busy).await.unwrap(), Applied::Busy);
    assert_eq!(calls.apply(&write).await.unwrap(), Applied::Exists);
    assert_eq!(calls.load(other).await.unwrap().0, None);
    assert_eq!(calls.user_call(carol).await.unwrap(), None, "nothing taken");
}

#[tokio::test]
async fn a_stale_version_is_a_conflict() {
    let (_container, calls, mut admin) = redis().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    let call = Uuid::now_v7();
    let now = calls.now().await.unwrap();
    let write = create(call, "first", now + 1_000, 60_000, vec![alice, bob]);
    assert_eq!(calls.apply(&write).await.unwrap(), Applied::Ok);
    let update = Write {
        expected: Some(1),
        record: Some("second"),
        due_ms: now + 15_000,
        ..write.clone()
    };
    assert_eq!(calls.apply(&update).await.unwrap(), Applied::Ok);
    assert_eq!(
        calls.load(call).await.unwrap().0,
        Some((2, "second".to_owned()))
    );
    assert_eq!(
        due_score(&mut admin, call).await,
        Some((now + 15_000) as f64)
    );
    let stale = Write {
        record: Some("third"),
        ..update
    };
    assert_eq!(calls.apply(&stale).await.unwrap(), Applied::Conflict);
    assert_eq!(
        calls.load(call).await.unwrap().0,
        Some((2, "second".to_owned()))
    );

    // A Redis that lost its scripts (a restart or SCRIPT FLUSH) is loaded again.
    let _: () = redis::cmd("SCRIPT")
        .arg("FLUSH")
        .query_async(&mut admin)
        .await
        .unwrap();
    let current = Write {
        expected: Some(2),
        ..stale
    };
    assert_eq!(calls.apply(&current).await.unwrap(), Applied::Ok);
    assert_eq!(
        calls.load(call).await.unwrap().0,
        Some((3, "third".to_owned()))
    );
}

#[tokio::test]
async fn an_update_refreshes_its_locks_but_never_takes_another_calls() {
    let (_container, calls, mut admin) = redis().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    let call = Uuid::now_v7();
    let now = calls.now().await.unwrap();
    let write = create(call, "first", now + 1_000, 1_000, vec![alice, bob]);
    assert_eq!(calls.apply(&write).await.unwrap(), Applied::Ok);
    let foreign = Uuid::now_v7().to_string();
    let _: () = redis::cmd("SET")
        .arg(user_key(bob))
        .arg(&foreign)
        .query_async(&mut admin)
        .await
        .unwrap();
    let update = Write {
        expected: Some(1),
        record: Some("second"),
        ttl_ms: 120_000,
        ..write
    };
    assert_eq!(calls.apply(&update).await.unwrap(), Applied::Ok);
    let ttl: i64 = redis::cmd("PTTL")
        .arg(user_key(alice))
        .query_async(&mut admin)
        .await
        .unwrap();
    assert!(ttl > 100_000, "refreshed: {ttl}");
    assert_eq!(
        calls.user_call(bob).await.unwrap(),
        Some(Uuid::parse_str(&foreign).unwrap())
    );
}

#[tokio::test]
async fn ending_releases_the_locks_and_the_due_entry() {
    let (_container, calls, mut admin) = redis().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    let call = Uuid::now_v7();
    let now = calls.now().await.unwrap();
    let write = create(call, "ringing", now + 45_000, 105_000, vec![alice, bob]);
    assert_eq!(calls.apply(&write).await.unwrap(), Applied::Ok);
    // Bob's lock names another call by now: an end must leave it.
    let foreign = Uuid::now_v7();
    let _: () = redis::cmd("SET")
        .arg(user_key(bob))
        .arg(foreign.to_string())
        .query_async(&mut admin)
        .await
        .unwrap();
    let end = Write {
        call,
        expected: Some(1),
        record: None,
        ttl_ms: 0,
        due_ms: 0,
        lock: Vec::new(),
        release: vec![alice, bob],
    };
    assert_eq!(calls.apply(&end).await.unwrap(), Applied::Ok);
    assert_eq!(calls.load(call).await.unwrap().0, None);
    assert!(!exists(&mut admin, &call_key(call)).await);
    assert_eq!(calls.user_call(alice).await.unwrap(), None);
    assert_eq!(calls.user_call(bob).await.unwrap(), Some(foreign));
    assert_eq!(due_score(&mut admin, call).await, None);
    assert_eq!(calls.live_count().await.unwrap(), 0);
    // Ending again is a conflict: the call is gone.
    assert_eq!(calls.apply(&end).await.unwrap(), Applied::Conflict);
}

#[tokio::test]
async fn claim_leases_due_calls_for_five_seconds() {
    let (_container, calls, mut admin) = redis().await;
    let now = calls.now().await.unwrap();
    let (due, later) = (Uuid::now_v7(), Uuid::now_v7());
    let users = [UserId::new(), UserId::new(), UserId::new(), UserId::new()];
    let first = create(due, "due", now - 10, 60_000, vec![users[0], users[1]]);
    assert_eq!(calls.apply(&first).await.unwrap(), Applied::Ok);
    let second = create(
        later,
        "later",
        now + 60_000,
        60_000,
        vec![users[2], users[3]],
    );
    assert_eq!(calls.apply(&second).await.unwrap(), Applied::Ok);

    let (claimed_at, ids) = calls.claim(32, 5_000).await.unwrap();
    assert!(claimed_at >= now);
    assert_eq!(ids, [due]);
    assert_eq!(
        due_score(&mut admin, due).await,
        Some((claimed_at + 5_000) as f64)
    );
    let (_, again) = calls.claim(32, 5_000).await.unwrap();
    assert!(again.is_empty(), "leased: {again:?}");
    assert_eq!(
        due_score(&mut admin, later).await,
        Some((now + 60_000) as f64)
    );

    // A due entry without a record is removed by the sweeper.
    calls.zrem_due(due).await.unwrap();
    assert_eq!(due_score(&mut admin, due).await, None);
}

#[tokio::test]
async fn keys_expire_without_message_svc() {
    let (_container, calls, mut admin) = redis().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    let call = Uuid::now_v7();
    let now = calls.now().await.unwrap();
    let write = create(call, "ringing", now + 100, 200, vec![alice, bob]);
    assert_eq!(calls.apply(&write).await.unwrap(), Applied::Ok);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(calls.load(call).await.unwrap().0, None);
    assert!(!exists(&mut admin, &call_key(call)).await);
    assert_eq!(calls.user_call(alice).await.unwrap(), None);
    assert_eq!(calls.user_call(bob).await.unwrap(), None);
}

#[tokio::test]
async fn detach_only_removes_the_matching_connection() {
    let (_container, calls, _admin) = redis().await;
    let (user, device) = (UserId::new(), DeviceId::new());
    let (old, new) = (Uuid::now_v7(), Uuid::now_v7());
    assert!(calls.attach(user, device, old).await.unwrap());
    assert_eq!(calls.connection_of(user, device).await.unwrap(), Some(old));
    // A takeover: the new connection replaces the old one.
    assert!(calls.attach(user, device, new).await.unwrap());
    assert_eq!(calls.connection_of(user, device).await.unwrap(), Some(new));
    assert!(!calls.detach(user, device, old).await.unwrap());
    assert_eq!(calls.connection_of(user, device).await.unwrap(), Some(new));
    assert!(calls.detach(user, device, new).await.unwrap());
    assert_eq!(calls.connection_of(user, device).await.unwrap(), None);
    assert!(!calls.detach(user, device, new).await.unwrap());
}

#[tokio::test]
async fn an_older_connection_never_replaces_a_newer_one() {
    let (_container, calls, _admin) = redis().await;
    let (user, device) = (UserId::new(), DeviceId::new());
    let (older, newer) = (Uuid::now_v7(), Uuid::now_v7());
    assert!(older.to_string() < newer.to_string());
    assert!(calls.attach(user, device, newer).await.unwrap());
    // The older connection's attach lands late, e.g. retried across the takeover.
    assert!(!calls.attach(user, device, older).await.unwrap());
    assert_eq!(
        calls.connection_of(user, device).await.unwrap(),
        Some(newer)
    );
    // Re-attaching the registered connection is fine.
    assert!(calls.attach(user, device, newer).await.unwrap());
    assert!(!calls.detach(user, device, older).await.unwrap());
    assert_eq!(
        calls.connection_of(user, device).await.unwrap(),
        Some(newer)
    );
    assert!(calls.detach(user, device, newer).await.unwrap());
    assert_eq!(calls.connection_of(user, device).await.unwrap(), None);
}

#[tokio::test]
async fn live_devices_need_a_presence_lease() {
    let (_container, calls, mut admin) = redis().await;
    let user = UserId::new();
    let (live, expired, unleased) = (DeviceId::new(), DeviceId::new(), DeviceId::new());
    let (live_conn, expired_conn) = (Uuid::new_v4(), Uuid::new_v4());
    calls.attach(user, live, live_conn).await.unwrap();
    calls.attach(user, expired, expired_conn).await.unwrap();
    calls.attach(user, unleased, Uuid::new_v4()).await.unwrap();
    let now = calls.now().await.unwrap();
    for (conn, end) in [(live_conn, now + 60_000), (expired_conn, now - 1_000)] {
        let _: () = redis::cmd("ZADD")
            .arg(presence_conns_key(user.0))
            .arg(end)
            .arg(conn.to_string())
            .query_async(&mut admin)
            .await
            .unwrap();
    }
    assert_eq!(calls.live_devices(user, now).await.unwrap(), [live]);
    assert!(calls.connection_live(user, live_conn, now).await.unwrap());
    assert!(
        !calls
            .connection_live(user, expired_conn, now)
            .await
            .unwrap()
    );
    assert!(
        calls
            .live_devices(UserId::new(), now)
            .await
            .unwrap()
            .is_empty()
    );
}
