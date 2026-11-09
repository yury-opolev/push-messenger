use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, RawQuery};
use axum::http::{HeaderMap, Method as HttpMethod, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use serde_json::{Value, json};

use super::*;

/// A stub auth-svc: records `METHOD path?query body` and answers from `answer`.
async fn serve(
    answer: fn(&str, &str) -> (StatusCode, Value),
) -> (AuthApi, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let router = Router::new().route(
        "/{*path}",
        any(
            move |method: HttpMethod,
                  Path(path): Path<String>,
                  RawQuery(query): RawQuery,
                  headers: HeaderMap,
                  body: String| {
                let record = Arc::clone(&record);
                async move {
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        "Bearer owner-token"
                    );
                    let line = format!(
                        "{method} /{path}{}{}",
                        query.map(|query| format!("?{query}")).unwrap_or_default(),
                        if body.is_empty() {
                            String::new()
                        } else {
                            format!(" {body}")
                        }
                    );
                    record.lock().unwrap().push(line);
                    let (status, body) = answer(method.as_str(), &path);
                    if status == StatusCode::NO_CONTENT {
                        return status.into_response();
                    }
                    (status, axum::Json(body)).into_response()
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (AuthApi::new(&url, None).unwrap(), seen)
}

const AGENT: &str = "0190f5c4-0000-7000-8000-000000000001";
const PEER: &str = "0190f5c4-0000-7000-8000-000000000002";

fn profile(id: &str, handle: &str) -> Value {
    json!({"user_id": id, "handle": handle, "display_name": handle, "kind": "user",
           "owner_user_id": null, "owner_display_name": null, "owner_handle": null,
           "deleted": false, "state": "active"})
}

fn owner_answers(method: &str, path: &str) -> (StatusCode, Value) {
    let rest = path.trim_start_matches(&format!("v1/agents/{AGENT}"));
    match (method, rest) {
        ("GET", "/policy") => (StatusCode::OK, json!({"policy": "ask"})),
        ("PUT", "/policy") => (StatusCode::OK, json!({"policy": "refuse_all"})),
        ("GET", "/requests") => (
            StatusCode::OK,
            json!({"requests": [{"user": profile(PEER, "peer"), "requested_at": "2026-11-06T10:00:00.000Z"}]}),
        ),
        ("GET", "/contacts") => (
            StatusCode::OK,
            json!({"contacts": [{"user": profile(PEER, "peer"), "state": "accepted", "owner": false}]}),
        ),
        ("GET", "/conversations") => (
            StatusCode::OK,
            json!({"conversations": [{"conversation_id": PEER, "kind": "direct", "title": null,
                   "peer": profile(PEER, "peer"), "peer_owner": null, "member_count": 2,
                   "last_activity_at": "2026-11-06T10:00:00.000Z", "message_count": 4,
                   "hidden": false, "peer_deleted": false, "started_by_agent": true}]}),
        ),
        ("GET", rest) if rest.ends_with("/messages") => (
            StatusCode::OK,
            json!({"entries": [{"seq": 1, "message_id": PEER, "client_msg_id": AGENT,
                   "kind": "message", "sender_id": PEER, "content_type": "text",
                   "body": "hi", "deleted": false, "target_seq": null, "answer_seq": null,
                   "client_ts": null, "server_ts": "2026-11-06T10:00:00.000Z",
                   "edited_at": null, "edit_seq": null}], "has_more": false}),
        ),
        ("GET", "/calls") => (StatusCode::OK, json!({"calls": []})),
        _ => (StatusCode::NO_CONTENT, Value::Null),
    }
}

#[tokio::test]
async fn the_owner_routes_are_called_and_read() {
    let (api, seen) = serve(owner_answers).await;
    let token = "owner-token";
    let agent = UserId(AGENT.parse().unwrap());
    let peer = UserId(PEER.parse().unwrap());
    assert_eq!(
        api.agent_policy(token, agent).await.unwrap(),
        AgentPolicy::Ask
    );
    assert_eq!(
        api.set_agent_policy(token, agent, AgentPolicy::RefuseAll)
            .await
            .unwrap(),
        AgentPolicy::RefuseAll
    );
    let requests = api.agent_requests(token, agent).await.unwrap();
    assert_eq!(requests[0].user.handle, "peer");
    assert_eq!(requests[0].requested_at_ms, 1_793_959_200_000);
    for decision in [Decision::Approve, Decision::Decline, Decision::Block] {
        api.decide(token, agent, peer, decision).await.unwrap();
    }
    let contacts = api.agent_contacts(token, agent).await.unwrap();
    assert_eq!(contacts[0].state, ContactState::Accepted);
    api.remove_agent_contact(token, agent, peer).await.unwrap();
    let conversations = api.agent_conversations(token, agent).await.unwrap();
    assert_eq!(conversations[0].message_count, 4);
    assert_eq!(conversations[0].peer.as_ref().unwrap().user_id, peer);
    assert!(conversations[0].started_by_agent);
    let page = api
        .agent_history(token, agent, ConversationId(peer.0), Some(9), Some(20))
        .await
        .unwrap();
    assert_eq!(page.entries[0].body.as_deref(), Some("hi"));
    assert!(!page.has_more);
    let calls = api
        .agent_calls(token, agent, Some("abc"), Some(5))
        .await
        .unwrap();
    assert!(calls.calls.is_empty());
    let base = format!("/v1/agents/{AGENT}");
    assert_eq!(
        *seen.lock().unwrap(),
        [
            format!("GET {base}/policy"),
            format!("PUT {base}/policy {{\"policy\":\"refuse_all\"}}"),
            format!("GET {base}/requests"),
            format!("POST {base}/requests/{PEER}/approve"),
            format!("POST {base}/requests/{PEER}/decline"),
            format!("POST {base}/requests/{PEER}/block"),
            format!("GET {base}/contacts"),
            format!("DELETE {base}/contacts/{PEER}"),
            format!("GET {base}/conversations"),
            format!("GET {base}/conversations/{PEER}/messages?before_seq=9&limit=20"),
            format!("GET {base}/calls?before=abc&limit=5"),
        ]
    );
}

#[tokio::test]
async fn a_non_owner_gets_not_found() {
    let (api, _) = serve(|_, _| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": {"code": "not_found", "message": "not found", "retryable": false}}),
        )
    })
    .await;
    let error = api
        .agent_conversations("owner-token", UserId(AGENT.parse().unwrap()))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "not_found");
}

const ACTION: &str = "0190f5c4-0000-7000-8000-0000000000aa";
const GROUP: &str = "0190f5c4-0000-7000-8000-0000000000bb";

fn action(state: &str, error: Option<&str>, resolved_at: Option<&str>) -> Value {
    json!({"action_id": ACTION, "kind": "group", "target": null, "conversation_id": GROUP,
           "title": "plans", "members": [profile(PEER, "peer")], "state": state,
           "error": error, "created_at": "2026-11-09T10:00:00.000Z",
           "expires_at": "2026-11-10T10:00:00.000Z", "resolved_at": resolved_at})
}

fn outgoing_answers(method: &str, path: &str) -> (StatusCode, Value) {
    let rest = path.trim_start_matches(&format!("v1/agents/{AGENT}"));
    match (method, rest) {
        ("GET", "/outgoing-policy") => (
            StatusCode::OK,
            json!({"requests": "ask", "chats": "ask", "groups": "off", "calls": "off"}),
        ),
        ("PUT", "/outgoing-policy") => (
            StatusCode::OK,
            json!({"requests": "ask", "chats": "allowed", "groups": "off", "calls": "ask"}),
        ),
        ("GET", "/inbound-policy") => (StatusCode::OK, json!({"policy": "owner_only"})),
        ("PUT", "/inbound-policy") => (StatusCode::OK, json!({"policy": "owner_agents"})),
        ("GET", "/outgoing") => (
            StatusCode::OK,
            json!({"actions": [
                action("waiting", None, None),
                {"action_id": ACTION, "kind": "request", "target": profile(PEER, "peer"),
                 "conversation_id": null, "title": null, "members": [], "state": "declined",
                 "error": null, "created_at": "2026-11-09T10:00:00.000Z",
                 "expires_at": "2026-11-10T10:00:00.000Z",
                 "resolved_at": "2026-11-09T11:00:00.000Z"},
                action("expired", None, Some("2026-11-10T10:00:00.000Z")),
            ]}),
        ),
        ("POST", rest) if rest.ends_with("/approve") => (
            StatusCode::OK,
            json!({"action": action("failed", Some("not_contact"), Some("2026-11-09T11:00:00.000Z"))}),
        ),
        _ => (StatusCode::NO_CONTENT, Value::Null),
    }
}

#[tokio::test]
async fn the_outgoing_and_inbound_routes_are_called_and_read() {
    let (api, seen) = serve(outgoing_answers).await;
    let token = "owner-token";
    let agent = UserId(AGENT.parse().unwrap());
    let peer = UserId(PEER.parse().unwrap());
    assert_eq!(
        api.outgoing_policy(token, agent).await.unwrap(),
        OutgoingPolicies {
            requests: OutgoingPolicy::Ask,
            chats: OutgoingPolicy::Ask,
            groups: OutgoingPolicy::Off,
            calls: OutgoingPolicy::Off,
        }
    );
    let update = OutgoingPolicyUpdate {
        chats: Some(OutgoingPolicy::Allowed),
        calls: Some(OutgoingPolicy::Ask),
        ..OutgoingPolicyUpdate::default()
    };
    let now = api.set_outgoing_policy(token, agent, update).await.unwrap();
    assert_eq!(
        (now.chats, now.calls),
        (OutgoingPolicy::Allowed, OutgoingPolicy::Ask)
    );
    assert_eq!(
        api.inbound_policy(token, agent).await.unwrap(),
        InboundPolicy::OwnerOnly
    );
    assert_eq!(
        api.set_inbound_policy(token, agent, InboundPolicy::OwnerAgents)
            .await
            .unwrap(),
        InboundPolicy::OwnerAgents
    );
    let actions = api.agent_outgoing(token, agent).await.unwrap();
    assert_eq!(
        actions[0],
        OwnerOutgoingAction {
            action_id: ACTION.to_owned(),
            kind: OutgoingKind::Group,
            target: None,
            conversation: Some(ConversationId(GROUP.parse().unwrap())),
            title: Some("plans".to_owned()),
            members: actions[0].members.clone(),
            state: OutgoingState::Waiting,
            error: None,
            created_at_ms: 1_794_218_400_000,
            expires_at_ms: 1_794_304_800_000,
            resolved_at_ms: None,
        }
    );
    assert_eq!(actions[0].members[0].user_id, peer);
    assert!(!format!("{:?}", actions[0]).contains("plans"));
    assert_eq!(actions[1].kind, OutgoingKind::Request);
    assert_eq!(actions[1].target.as_ref().unwrap().user_id, peer);
    assert_eq!(actions[1].state, OutgoingState::OwnerDeclined);
    assert_eq!(actions[1].resolved_at_ms, Some(1_794_222_000_000));
    assert_eq!(actions[2].state, OutgoingState::Expired);
    let approved = api.approve_outgoing(token, agent, ACTION).await.unwrap();
    assert_eq!(approved.state, OutgoingState::Failed);
    assert_eq!(approved.error.as_deref(), Some("not_contact"));
    api.decline_outgoing(token, agent, ACTION).await.unwrap();
    let base = format!("/v1/agents/{AGENT}");
    assert_eq!(
        *seen.lock().unwrap(),
        [
            format!("GET {base}/outgoing-policy"),
            format!("PUT {base}/outgoing-policy {{\"calls\":\"ask\",\"chats\":\"allowed\"}}"),
            format!("GET {base}/inbound-policy"),
            format!("PUT {base}/inbound-policy {{\"policy\":\"owner_agents\"}}"),
            format!("GET {base}/outgoing"),
            format!("POST {base}/outgoing/{ACTION}/approve"),
            format!("POST {base}/outgoing/{ACTION}/decline"),
        ]
    );
}

#[tokio::test]
async fn a_broken_action_id_is_refused_before_anything_is_sent() {
    let (api, seen) = serve(outgoing_answers).await;
    let agent = UserId(AGENT.parse().unwrap());
    let error = api
        .decline_outgoing("owner-token", agent, "../../policy")
        .await
        .unwrap_err();
    assert_eq!(error.code(), "invalid");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_action_that_no_longer_waits_is_not_found() {
    let (api, _) = serve(|_, _| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": {"code": "not_found", "message": "not found", "retryable": false}}),
        )
    })
    .await;
    let agent = UserId(AGENT.parse().unwrap());
    let error = api
        .approve_outgoing("owner-token", agent, ACTION)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "not_found");
    let error = api.outgoing_policy("owner-token", agent).await.unwrap_err();
    assert_eq!(error.code(), "not_found");
}
