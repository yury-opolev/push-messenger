//! The call history routes (voice calls spec §9): `GET /v1/calls` and
//! `DELETE /v1/calls/{call_id}`, with a fake call log in place of message-svc.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{HeaderName, Method, StatusCode};
use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

use super::templates::send;
use super::{Offer, World, capture_logs, password_user, security_events, world};
use crate::api::{Api, router};
use crate::calls::{CallCursor, CallEntry, CallLog, NoCalls};
use crate::directory::NoDirectory;
use crate::error::AuthError;
use crate::limits::{Limits, MemoryCounter};
use crate::templates::NoTemplates;

/// What a history request asked: the user, `before`, `since` and `limit`.
type Asked = (Uuid, Option<CallCursor>, Option<CallCursor>, u32);

/// message-svc's call log in miniature: per-user calls, newest first, filtered as
/// message-svc filters them; it records what it was asked.
#[derive(Debug, Default)]
struct FakeLog {
    calls: Mutex<HashMap<Uuid, Vec<CallEntry>>>,
    asked: Mutex<Vec<Asked>>,
    hidden: Mutex<Vec<(Uuid, Uuid)>>,
}

#[async_trait::async_trait]
impl CallLog for FakeLog {
    async fn history(
        &self,
        user: Uuid,
        before: Option<CallCursor>,
        since: Option<CallCursor>,
        limit: u32,
    ) -> Result<Vec<CallEntry>, AuthError> {
        self.asked
            .lock()
            .unwrap()
            .push((user, before, since, limit));
        let key = |cursor: &CallCursor| (cursor.ended_at, cursor.call_id);
        Ok(self
            .calls
            .lock()
            .unwrap()
            .get(&user)
            .into_iter()
            .flatten()
            .filter(|call| before.is_none_or(|before| key(&call.cursor) < key(&before)))
            .filter(|call| since.is_none_or(|since| key(&call.cursor) > key(&since)))
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn hide(&self, user: Uuid, call: Uuid) -> Result<(), AuthError> {
        self.hidden.lock().unwrap().push((user, call));
        Ok(())
    }
}

fn at(micros: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(micros).unwrap()
}

/// A call of `peer`'s that ended at `ended_at` (µs).
fn call(peer: Uuid, ended_at: i64, answered: bool) -> CallEntry {
    let call_id = Uuid::now_v7();
    CallEntry {
        call_id,
        peer_user_id: peer,
        direction: if answered { "out" } else { "in" },
        started_at: at(ended_at - 70_000_000),
        answered_at: answered.then(|| at(ended_at - 61_000_000)),
        ended_at: at(ended_at),
        duration_s: if answered { 61 } else { 0 },
        outcome: if answered { "completed" } else { "missed" },
        started_by_agent: answered,
        cursor: CallCursor { ended_at, call_id },
    }
}

fn public(world: &World, call_history: Arc<dyn CallLog>) -> Router {
    router(Api {
        auth: world.auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::new(NoDirectory),
        templates: Arc::new(NoTemplates),
        call_history,
        agent_owners: Arc::new(crate::owners::NoAgentOwners),
        turn: Arc::new(crate::turn::Turn::disabled()),
    })
}

#[tokio::test]
async fn the_history_lists_the_callers_own_calls_page_by_page() {
    let world = world(Offer::Password).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 1)
        .await
        .remove(0);
    let bob = password_user(&world.auth, &world.pool, "bob", 1)
        .await
        .remove(0);
    let log = Arc::new(FakeLog::default());
    let newest = call(bob.user_id, 1_790_000_300_000_000, true);
    let middle = call(bob.user_id, 1_790_000_200_000_000, false);
    let oldest = call(bob.user_id, 1_790_000_100_000_000, false);
    log.calls.lock().unwrap().insert(
        alice.user_id,
        vec![newest.clone(), middle.clone(), oldest.clone()],
    );
    log.calls.lock().unwrap().insert(
        bob.user_id,
        vec![call(alice.user_id, 1_790_000_300_000_000, true)],
    );
    let routes = public(&world, log.clone());

    let token = Some(alice.access_token.as_str());
    let answer = send(&routes, Method::GET, "/v1/calls", token, &[], None).await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.json());
    assert_eq!(answer.headers["cache-control"], "no-store");
    let page = answer.json();
    assert_eq!(page["calls"].as_array().unwrap().len(), 3);
    assert_eq!(
        page["calls"][0],
        json!({
            "call_id": newest.call_id,
            "peer_user_id": bob.user_id,
            "direction": "out",
            "started_at": "2026-09-21T14:17:10.000Z",
            "answered_at": "2026-09-21T14:17:19.000Z",
            "ended_at": "2026-09-21T14:18:20.000Z",
            "duration_s": 61,
            "outcome": "completed",
            "started_by_agent": true,
            "cursor": newest.cursor.encode(),
        })
    );
    assert!(page["calls"][1].get("answered_at").is_none());
    assert_eq!(page["calls"][1]["started_by_agent"], false);
    assert_eq!(page["calls"][1]["outcome"], "missed");
    assert!(page.get("next").is_none(), "the page is not full");
    assert_eq!(
        log.asked.lock().unwrap().last(),
        Some(&(alice.user_id, None, None, 50))
    );

    // A full page names the next one.
    let answer = send(&routes, Method::GET, "/v1/calls?limit=2", token, &[], None).await;
    let page = answer.json();
    assert_eq!(page["calls"].as_array().unwrap().len(), 2);
    assert_eq!(page["next"], middle.cursor.encode());
    let next = format!("/v1/calls?limit=2&before={}", middle.cursor.encode());
    let page = send(&routes, Method::GET, &next, token, &[], None)
        .await
        .json();
    assert_eq!(page["calls"][0]["call_id"], json!(oldest.call_id));
    assert!(page.get("next").is_none());
    assert_eq!(
        log.asked.lock().unwrap().last(),
        Some(&(alice.user_id, Some(middle.cursor), None, 2))
    );

    // `since`: only newer calls, for a missed-call badge.
    let since = format!("/v1/calls?since={}", oldest.cursor.encode());
    let page = send(&routes, Method::GET, &since, token, &[], None)
        .await
        .json();
    let ids: Vec<_> = page["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["call_id"].clone())
        .collect();
    assert_eq!(ids, [json!(newest.call_id), json!(middle.call_id)]);

    // Bob's token shows bob's calls only: the user comes from the token, never the query.
    let other = format!("/v1/calls?user_id={}", alice.user_id);
    let answer = send(
        &routes,
        Method::GET,
        &other,
        Some(&bob.access_token),
        &[],
        None,
    )
    .await;
    assert_eq!(answer.json()["calls"].as_array().unwrap().len(), 1);
    assert_eq!(
        answer.json()["calls"][0]["peer_user_id"],
        json!(alice.user_id)
    );
    assert_eq!(log.asked.lock().unwrap().last().unwrap().0, bob.user_id);
}

#[tokio::test]
async fn bad_queries_and_missing_tokens_are_refused() {
    let world = world(Offer::Password).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 1)
        .await
        .remove(0);
    let log = Arc::new(FakeLog::default());
    let routes = public(&world, log.clone());
    let token = Some(alice.access_token.as_str());
    let uncanonical = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        format!("0012:{}", Uuid::nil()),
    );
    for (query, field) in [
        ("limit=0", "limit"),
        ("limit=101", "limit"),
        ("limit=ten", "limit"),
        ("before=not-a-cursor", "before"),
        ("since=bm90IGEgY3Vyc29y", "since"),
        (&format!("before={uncanonical}") as &str, "before"),
        ("limit=1&limit=2", "query"),
    ] {
        let answer = send(
            &routes,
            Method::GET,
            &format!("/v1/calls?{query}"),
            token,
            &[],
            None,
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(answer.json()["error"]["code"], "invalid", "{query}");
        assert!(
            answer.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains(field),
            "{query}: {:?}",
            answer.json()
        );
    }
    assert!(log.asked.lock().unwrap().is_empty());
    for (method, uri) in [
        (Method::GET, "/v1/calls".to_owned()),
        (Method::DELETE, format!("/v1/calls/{}", Uuid::now_v7())),
    ] {
        let answer = send(&routes, method, &uri, None, &[], None).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let answer = send(
        &routes,
        Method::DELETE,
        "/v1/calls/not-a-call",
        token,
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(log.hidden.lock().unwrap().is_empty());

    // Without message-svc: unavailable, retryable.
    let routes = public(&world, Arc::new(NoCalls));
    let answer = send(&routes, Method::GET, "/v1/calls", token, &[], None).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.json()["error"]["retryable"], true);
}

#[tokio::test]
async fn hiding_a_call_names_the_caller_only_and_is_audited() {
    capture_logs();
    let world = world(Offer::Password).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 1)
        .await
        .remove(0);
    let log = Arc::new(FakeLog::default());
    let routes = public(&world, log.clone());
    let call = Uuid::now_v7();

    let answer = send(
        &routes,
        Method::DELETE,
        &format!("/v1/calls/{call}"),
        Some(&alice.access_token),
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::NO_CONTENT);
    assert!(answer.body.is_empty());
    assert_eq!(*log.hidden.lock().unwrap(), vec![(alice.user_id, call)]);
    assert!(security_events().iter().any(|event| {
        event["event"] == "calls.history.hidden"
            && event["user_id"] == alice.user_id.to_string()
            && event["route"] == "/v1/calls/{call_id}"
    }));
    // Reads are not security events.
    send(
        &routes,
        Method::GET,
        "/v1/calls",
        Some(&alice.access_token),
        &[],
        None,
    )
    .await;
    assert!(!security_events().iter().any(
        |event| event["route"] == "/v1/calls" && event["user_id"] == alice.user_id.to_string()
    ));
}
