//! The owner routes of an agent (spec 2026-11-06 agent connections §5.1) over a stub of
//! message-svc's `AgentOwners` API.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use super::agents::{app_token, call, setup};
use crate::AuthError;
use crate::api::{Api, router};
use crate::calls::{CallCursor, CallEntry};
use crate::entra::testing::TENANT;
use crate::limits::{Limits, MemoryCounter};
use crate::owners::{AgentOwnerApi, Resolve};
use crate::service::Auth;
use push_proto::id_bytes;
use push_proto::internal::v1 as internal;
use push_proto::v1::{ContentType, ConversationKind, Deliver, EntryKind, HistoryPage};

/// Records every call; answers from what it holds.
#[derive(Debug, Default)]
struct Stub {
    calls: Mutex<Vec<String>>,
    policy: Mutex<&'static str>,
    waiting: Vec<(Uuid, DateTime<Utc>)>,
    contacts: Vec<(Uuid, &'static str)>,
    owner: Mutex<Option<Uuid>>,
    conversations: Vec<internal::AgentConversation>,
    page: HistoryPage,
}

impl Stub {
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl AgentOwnerApi for Stub {
    async fn policy(&self, _: Uuid, _: Uuid) -> Result<&'static str, AuthError> {
        self.record("policy".into());
        Ok(*self.policy.lock().unwrap())
    }
    async fn set_policy(&self, _: Uuid, _: Uuid, policy: &str) -> Result<&'static str, AuthError> {
        self.record(format!("set_policy {policy}"));
        let known = crate::owners::POLICIES
            .into_iter()
            .find(|known| *known == policy)
            .ok_or(AuthError::Invalid(crate::rules::Invalid {
                field: "policy",
                reason: "unknown",
            }))?;
        *self.policy.lock().unwrap() = known;
        Ok(known)
    }
    async fn waiting(&self, _: Uuid, _: Uuid) -> Result<Vec<(Uuid, DateTime<Utc>)>, AuthError> {
        self.record("waiting".into());
        Ok(self.waiting.clone())
    }
    async fn resolve(
        &self,
        _: Uuid,
        _: Uuid,
        user: Uuid,
        action: Resolve,
    ) -> Result<(), AuthError> {
        self.record(format!("resolve {user} {action:?}"));
        if Some(user) == *self.owner.lock().unwrap() {
            return Err(AuthError::OwnerContact);
        }
        Ok(())
    }
    async fn contacts(&self, _: Uuid, _: Uuid) -> Result<Vec<(Uuid, &'static str)>, AuthError> {
        self.record("contacts".into());
        Ok(self.contacts.clone())
    }
    async fn remove(&self, _: Uuid, _: Uuid, user: Uuid) -> Result<(), AuthError> {
        self.record(format!("remove {user}"));
        if Some(user) == *self.owner.lock().unwrap() {
            return Err(AuthError::OwnerContact);
        }
        Ok(())
    }
    async fn conversations(
        &self,
        _: Uuid,
        _: Uuid,
    ) -> Result<Vec<internal::AgentConversation>, AuthError> {
        self.record("conversations".into());
        Ok(self.conversations.clone())
    }
    async fn history(
        &self,
        _: Uuid,
        _: Uuid,
        conversation: Uuid,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<HistoryPage, AuthError> {
        self.record(format!("history {conversation} {before_seq:?} {limit}"));
        Ok(self.page.clone())
    }
    async fn calls(
        &self,
        _: Uuid,
        _: Uuid,
        before: Option<CallCursor>,
        limit: u32,
    ) -> Result<Vec<CallEntry>, AuthError> {
        self.record(format!("calls {} {limit}", before.is_some()));
        Ok(Vec::new())
    }
}

/// message-svc's answer: the agent sees everyone but `hidden`.
#[derive(Debug)]
struct AllBut(Vec<Uuid>);

#[async_trait::async_trait]
impl crate::Directory for AllBut {
    async fn visible_users(&self, _: Uuid, candidates: &[Uuid]) -> Result<Vec<Uuid>, AuthError> {
        Ok(candidates
            .iter()
            .copied()
            .filter(|user| !self.0.contains(user))
            .collect())
    }
}

fn with_owners(auth: Auth, owners: Arc<Stub>) -> Router {
    with_owners_hiding(auth, owners, Vec::new())
}

fn with_owners_hiding(auth: Auth, owners: Arc<Stub>, hidden: Vec<Uuid>) -> Router {
    router(Api {
        auth,
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: None,
        directory: Arc::new(AllBut(hidden)),
        templates: Arc::new(crate::templates::NoTemplates),
        call_history: Arc::new(crate::calls::NoCalls),
        agent_owners: owners,
        turn: Arc::new(crate::turn::Turn::disabled()),
    })
}

/// Every owner route of `agent`, with a method and a body.
fn routes(agent: &str, user: &str) -> Vec<(&'static str, String, Value)> {
    vec![
        ("GET", format!("/v1/agents/{agent}/policy"), json!({})),
        (
            "PUT",
            format!("/v1/agents/{agent}/policy"),
            json!({"policy": "auto_any"}),
        ),
        ("GET", format!("/v1/agents/{agent}/requests"), json!({})),
        (
            "POST",
            format!("/v1/agents/{agent}/requests/{user}/approve"),
            json!({}),
        ),
        (
            "POST",
            format!("/v1/agents/{agent}/requests/{user}/decline"),
            json!({}),
        ),
        (
            "POST",
            format!("/v1/agents/{agent}/requests/{user}/block"),
            json!({}),
        ),
        ("GET", format!("/v1/agents/{agent}/contacts"), json!({})),
        (
            "GET",
            format!("/v1/agents/{agent}/conversations"),
            json!({}),
        ),
        (
            "GET",
            format!("/v1/agents/{agent}/conversations/{user}/messages"),
            json!({}),
        ),
        ("GET", format!("/v1/agents/{agent}/calls"), json!({})),
        (
            "DELETE",
            format!("/v1/agents/{agent}/contacts/{user}"),
            json!({}),
        ),
    ]
}

#[tokio::test]
async fn everyone_but_the_owner_finds_nothing_and_message_svc_is_never_asked() {
    let (world, auth, plain, owner) = setup().await;
    let stub = Arc::new(Stub::default());
    let api = with_owners(auth.clone(), Arc::clone(&stub));
    let (status, reservation) = call(
        &plain,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle":"watched","display_name":"Watched"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reservation}");
    let token = app_token(&world, TENANT, Uuid::new_v4());
    let (status, registered) = call(
        &plain,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token,"enrollment_code":reservation["enrollment_code"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{registered}");
    let agent = registered["user_id"].as_str().unwrap().to_owned();
    let device = auth
        .register_device(
            registered["registration_token"].as_str().unwrap(),
            Uuid::now_v7(),
            "Agent",
            "other",
        )
        .await
        .unwrap();
    let stranger = super::password_user(&auth, &world.pool, "stranger", 1)
        .await
        .remove(0);
    let user = Uuid::now_v7().to_string();
    let unknown = Uuid::now_v7().to_string();
    let mut cases = Vec::new();
    for (method, path, body) in routes(&agent, &user) {
        cases.push((
            stranger.access_token.clone(),
            method,
            path.clone(),
            body.clone(),
        ));
        // The agent itself is not its owner.
        cases.push((device.access_token.clone(), method, path, body));
    }
    for (method, path, body) in routes(&unknown, &user)
        .into_iter()
        .chain(routes("not-an-id", &user))
    {
        cases.push((owner.access_token.clone(), method, path, body));
    }
    for (token, method, path, body) in cases {
        let (status, answer) = call(&api, method, &path, &token, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {answer}");
        assert_eq!(answer["error"]["code"], "not_found", "{method} {path}");
    }
    assert_eq!(stub.calls(), Vec::<String>::new());

    // Deleting the agent ends its owner's routes too.
    let (status, _) = call(
        &api,
        "GET",
        &format!("/v1/agents/{agent}/policy"),
        &owner.access_token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &plain,
        "DELETE",
        &format!("/v1/agents/{agent}"),
        &owner.access_token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, answer) = call(
        &api,
        "GET",
        &format!("/v1/agents/{agent}/policy"),
        &owner.access_token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    // Without a token: 401, as everywhere.
    let (status, _) = call(
        &api,
        "GET",
        &format!("/v1/agents/{agent}/policy"),
        "",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_owner_reads_and_sets_the_policy_and_acts_on_requests() {
    let (world, auth, plain, owner) = setup().await;
    let (_, reservation) = call(
        &plain,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle":"helper","display_name":"Helper"}),
    )
    .await;
    let agent = reservation["user_id"].as_str().unwrap().to_owned();
    let asking = super::password_user(&auth, &world.pool, "asking", 1)
        .await
        .remove(0);
    let unseen = super::password_user(&auth, &world.pool, "unseen", 1)
        .await
        .remove(0);
    let at = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
    let stub = Arc::new(Stub {
        policy: Mutex::new("ask"),
        waiting: vec![(asking.user_id, at)],
        contacts: vec![
            (owner.user_id, "accepted"),
            (asking.user_id, "requested_in"),
            (unseen.user_id, "requested_out"),
        ],
        owner: Mutex::new(Some(owner.user_id)),
        ..Stub::default()
    });
    // The agent asked `unseen`, who never answered: the agent may not see their profile,
    // so neither may its owner.
    let api = with_owners_hiding(auth.clone(), Arc::clone(&stub), vec![unseen.user_id]);
    let token = owner.access_token.as_str();
    let base = format!("/v1/agents/{agent}");

    let (status, policy) = call(&api, "GET", &format!("{base}/policy"), token, json!({})).await;
    assert_eq!((status, policy), (StatusCode::OK, json!({"policy": "ask"})));
    let (status, policy) = call(
        &api,
        "PUT",
        &format!("{base}/policy"),
        token,
        json!({"policy": "auto_owner_contacts"}),
    )
    .await;
    assert_eq!(
        (status, policy),
        (StatusCode::OK, json!({"policy": "auto_owner_contacts"}))
    );
    let (status, bad) = call(
        &api,
        "PUT",
        &format!("{base}/policy"),
        token,
        json!({"policy": "everyone"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    assert_eq!(bad["error"]["code"], "invalid");

    let (status, requests) = call(&api, "GET", &format!("{base}/requests"), token, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{requests}");
    assert_eq!(requests["requests"][0]["user"]["handle"], "asking");
    assert_eq!(
        requests["requests"][0]["user"]["user_id"],
        asking.user_id.to_string()
    );
    assert_eq!(
        requests["requests"][0]["requested_at"],
        "2026-09-21T14:13:20.000Z"
    );

    let (status, contacts) = call(&api, "GET", &format!("{base}/contacts"), token, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{contacts}");
    assert_eq!(contacts["contacts"][0]["owner"], true);
    assert_eq!(contacts["contacts"][1]["owner"], false);
    assert_eq!(contacts["contacts"][1]["state"], "requested_in");
    assert_eq!(
        contacts["contacts"][2]["user"],
        json!({"user_id": unseen.user_id, "handle": "", "display_name": "", "kind": "user",
               "owner_user_id": null, "owner_display_name": null, "owner_handle": null,
               "deleted": false, "state": "hidden"})
    );
    assert_eq!(contacts["contacts"][2]["state"], "requested_out");

    let requester = asking.user_id;
    for action in ["approve", "decline", "block"] {
        let (status, _) = call(
            &api,
            "POST",
            &format!("{base}/requests/{requester}/{action}"),
            token,
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{action}");
    }
    let (status, _) = call(
        &api,
        "DELETE",
        &format!("{base}/contacts/{requester}"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let me = owner.user_id;
    let (status, refused) = call(
        &api,
        "DELETE",
        &format!("{base}/contacts/{me}"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"]["code"], "owner_contact");
    let (status, _) = call(
        &api,
        "POST",
        &format!("{base}/requests/not-an-id/approve"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        stub.calls(),
        [
            "policy".to_owned(),
            "set_policy auto_owner_contacts".to_owned(),
            "set_policy everyone".to_owned(),
            "waiting".to_owned(),
            "contacts".to_owned(),
            format!("resolve {requester} Approve"),
            format!("resolve {requester} Decline"),
            format!("resolve {requester} Block"),
            format!("remove {requester}"),
            format!("remove {me}"),
        ]
    );
}

#[tokio::test]
async fn the_owner_view_shows_the_agents_chats_and_has_no_write_path() {
    super::capture_logs();
    let (world, auth, plain, owner) = setup().await;
    let (_, reservation) = call(
        &plain,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle":"viewed","display_name":"Viewed"}),
    )
    .await;
    let agent = reservation["user_id"].as_str().unwrap().to_owned();
    let peer = super::password_user(&auth, &world.pool, "peer", 1)
        .await
        .remove(0);
    let chat = Uuid::now_v7();
    let (message, client) = (Uuid::now_v7(), Uuid::now_v7());
    let stub = Arc::new(Stub {
        conversations: vec![internal::AgentConversation {
            conversation_id: id_bytes(chat),
            kind: ConversationKind::Direct.into(),
            title: None,
            peer_user_id: Some(id_bytes(peer.user_id)),
            peer_owner_user_id: None,
            member_count: 2,
            last_activity_at: 1_790_000_000_000_000,
            message_count: 7,
            hidden: false,
            peer_deleted: false,
        }],
        page: HistoryPage {
            entries: vec![Deliver {
                conversation_id: id_bytes(chat),
                seq: 3,
                message_id: id_bytes(message),
                client_msg_id: id_bytes(client),
                kind: EntryKind::Message.into(),
                sender_id: id_bytes(peer.user_id),
                content_type: ContentType::Text.into(),
                body: Some(b"secret words".to_vec()),
                server_ts: 1_790_000_000_000_000,
                ..Deliver::default()
            }],
            has_more: true,
        },
        ..Stub::default()
    });
    let api = with_owners(auth.clone(), Arc::clone(&stub));
    let token = owner.access_token.as_str();
    let base = format!("/v1/agents/{agent}");

    let (status, listed) = call(
        &api,
        "GET",
        &format!("{base}/conversations"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let first = &listed["conversations"][0];
    assert_eq!(first["conversation_id"], chat.to_string());
    assert_eq!(first["kind"], "direct");
    assert_eq!(first["peer"]["handle"], "peer");
    assert_eq!(first["peer_owner"], Value::Null);
    assert_eq!(first["message_count"], 7);
    assert_eq!(first["last_activity_at"], "2026-09-21T14:13:20.000Z");

    let path = format!("{base}/conversations/{chat}/messages?before_seq=9&limit=20");
    let (status, page) = call(&api, "GET", &path, token, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["has_more"], true);
    assert_eq!(
        page["entries"][0],
        json!({"seq": 3, "message_id": message, "client_msg_id": client, "kind": "message",
               "sender_id": peer.user_id, "content_type": "text", "body": "secret words",
               "deleted": false, "target_seq": null, "answer_seq": null, "client_ts": null,
               "server_ts": "2026-09-21T14:13:20.000Z", "edited_at": null, "edit_seq": null})
    );
    let (status, calls) = call(
        &api,
        "GET",
        &format!("{base}/calls?limit=10"),
        token,
        json!({}),
    )
    .await;
    assert_eq!((status, calls), (StatusCode::OK, json!({"calls": []})));
    let (status, _) = call(
        &api,
        "GET",
        &format!("{base}/conversations/{chat}/messages?limit=0"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // There is no way to write as the agent through the view.
    for (method, path) in [
        ("POST", format!("{base}/conversations")),
        ("POST", format!("{base}/conversations/{chat}/messages")),
        ("PUT", format!("{base}/conversations/{chat}/messages")),
        ("PATCH", format!("{base}/conversations/{chat}/messages")),
        ("DELETE", format!("{base}/conversations/{chat}/messages")),
        ("POST", format!("{base}/calls")),
    ] {
        let (status, _) = call(&api, method, &path, token, json!({"body": "hi"})).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
    }
    // Every read is audited with ids and counts, never the content.
    let events: Vec<Value> = super::security_events()
        .into_iter()
        .filter(|event| event["agent_id"] == agent.as_str())
        .collect();
    let named = |name: &str| {
        events
            .iter()
            .find(|event| event["event"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} in {events:?}"))
    };
    let read = named("agent.view.history");
    assert_eq!(read["conversation_id"], chat.to_string());
    assert_eq!(read["count"], 1);
    assert_eq!(read["user_id"], owner.user_id.to_string());
    assert_eq!(named("agent.view.conversations")["count"], 1);
    assert_eq!(named("agent.view.calls")["count"], 0);
    assert!(!super::captured_logs().contains("secret words"));
    assert_eq!(
        stub.calls(),
        [
            "conversations".to_owned(),
            format!("history {chat} Some(9) 20"),
            "calls false 10".to_owned(),
        ]
    );
}
