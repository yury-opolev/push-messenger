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
use crate::owners::{AgentOwnerApi, AgentSettings, OutgoingAction, Resolve};
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
    call_log: Vec<CallEntry>,
    settings: Mutex<AgentSettings>,
    /// The waiting actions; a resolved one leaves.
    outgoing: Mutex<Vec<OutgoingAction>>,
    /// The action whose approval the normal rules refuse (`not_contact`).
    failing: Option<Uuid>,
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
        Ok(self.call_log.clone())
    }
    async fn settings(&self, _: Uuid, _: Uuid) -> Result<AgentSettings, AuthError> {
        self.record("settings".into());
        Ok(*self.settings.lock().unwrap())
    }
    async fn set_outgoing(
        &self,
        _: Uuid,
        _: Uuid,
        change: [Option<&'static str>; 4],
    ) -> Result<AgentSettings, AuthError> {
        self.record(format!("set_outgoing {change:?}"));
        let mut settings = self.settings.lock().unwrap();
        for (now, new) in settings.outgoing.iter_mut().zip(change) {
            if let Some(new) = new {
                *now = new;
            }
        }
        Ok(*settings)
    }
    async fn set_inbound(
        &self,
        _: Uuid,
        _: Uuid,
        policy: &'static str,
    ) -> Result<AgentSettings, AuthError> {
        self.record(format!("set_inbound {policy}"));
        let mut settings = self.settings.lock().unwrap();
        settings.inbound = policy;
        Ok(*settings)
    }
    async fn outgoing(&self, _: Uuid, _: Uuid) -> Result<Vec<OutgoingAction>, AuthError> {
        self.record("outgoing".into());
        Ok(self.outgoing.lock().unwrap().clone())
    }
    async fn resolve_outgoing(
        &self,
        _: Uuid,
        _: Uuid,
        action: Uuid,
        approve: bool,
    ) -> Result<OutgoingAction, AuthError> {
        self.record(format!("resolve_outgoing {action} {approve}"));
        let mut waiting = self.outgoing.lock().unwrap();
        let at = waiting
            .iter()
            .position(|waiting| waiting.action_id == action)
            .ok_or(AuthError::NotFound)?;
        let mut resolved = waiting.remove(at);
        resolved.resolved_at = Some(resolved.created_at + chrono::TimeDelta::minutes(5));
        resolved.state = match (approve, self.failing == Some(action)) {
            (false, _) => "declined",
            (true, false) => "approved",
            (true, true) => {
                resolved.error = Some("not_contact".to_owned());
                "failed"
            }
        };
        Ok(resolved)
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
        (
            "GET",
            format!("/v1/agents/{agent}/outgoing-policy"),
            json!({}),
        ),
        (
            "PUT",
            format!("/v1/agents/{agent}/outgoing-policy"),
            json!({"chats": "allowed"}),
        ),
        (
            "GET",
            format!("/v1/agents/{agent}/inbound-policy"),
            json!({}),
        ),
        (
            "PUT",
            format!("/v1/agents/{agent}/inbound-policy"),
            json!({"policy": "contacts"}),
        ),
        ("GET", format!("/v1/agents/{agent}/outgoing"), json!({})),
        (
            "POST",
            format!("/v1/agents/{agent}/outgoing/{user}/approve"),
            json!({}),
        ),
        (
            "POST",
            format!("/v1/agents/{agent}/outgoing/{user}/decline"),
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
    for (method, path, body) in routes(&agent, &user) {
        let (status, answer) = call(&api, method, &path, &owner.access_token, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {answer}");
    }
    assert_eq!(stub.calls(), ["policy".to_owned()]);
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
            started_by_agent: true,
        }],
        call_log: vec![CallEntry {
            call_id: chat,
            peer_user_id: peer.user_id,
            direction: "out",
            started_at: DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            answered_at: None,
            ended_at: DateTime::from_timestamp(1_790_000_030, 0).unwrap(),
            duration_s: 0,
            outcome: "missed",
            started_by_agent: true,
            cursor: CallCursor {
                ended_at: 1_790_000_030_000_000,
                call_id: chat,
            },
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
    assert_eq!(first["started_by_agent"], true);

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
    assert_eq!(status, StatusCode::OK, "{calls}");
    assert_eq!(calls["calls"][0]["call_id"], chat.to_string());
    assert_eq!(calls["calls"][0]["started_by_agent"], true);
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
    assert_eq!(named("agent.view.calls")["count"], 1);
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

/// An owner and their agent, with a router over `stub`.
async fn owned(handle: &str, stub: Arc<Stub>) -> (super::World, Auth, Router, String, String) {
    let (world, auth, plain, owner) = setup().await;
    let (status, reservation) = call(
        &plain,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle": handle, "display_name": handle}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reservation}");
    let agent = reservation["user_id"].as_str().unwrap().to_owned();
    let api = with_owners(auth.clone(), stub);
    (world, auth, api, owner.access_token, agent)
}

#[tokio::test]
async fn the_owner_reads_and_sets_the_outgoing_and_inbound_policies() {
    super::capture_logs();
    let stub = Arc::new(Stub::default());
    let (_world, _auth, api, token, agent) = owned("governed", Arc::clone(&stub)).await;
    let token = token.as_str();
    let outgoing = format!("/v1/agents/{agent}/outgoing-policy");
    let inbound = format!("/v1/agents/{agent}/inbound-policy");

    let (status, policy) = call(&api, "GET", &outgoing, token, json!({})).await;
    assert_eq!(
        (status, policy),
        (
            StatusCode::OK,
            json!({"requests": "ask", "chats": "ask", "groups": "off", "calls": "off"})
        )
    );
    // A subset changes those given and keeps the others.
    let (status, policy) = call(
        &api,
        "PUT",
        &outgoing,
        token,
        json!({"chats": "allowed", "calls": "ask"}),
    )
    .await;
    assert_eq!(
        (status, policy),
        (
            StatusCode::OK,
            json!({"requests": "ask", "chats": "allowed", "groups": "off", "calls": "ask"})
        )
    );
    // Nothing given changes nothing.
    let (status, policy) = call(&api, "PUT", &outgoing, token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["chats"], "allowed");
    for bad in [
        json!({"chats": "maybe"}),
        json!({"messages": "allowed"}),
        json!({"chats": 1}),
        json!(["chats"]),
    ] {
        let (status, refused) = call(&api, "PUT", &outgoing, token, bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {refused}");
        assert_eq!(refused["error"]["code"], "invalid", "{bad}");
    }

    let (status, policy) = call(&api, "GET", &inbound, token, json!({})).await;
    assert_eq!(
        (status, policy),
        (StatusCode::OK, json!({"policy": "owner_only"}))
    );
    let (status, policy) = call(
        &api,
        "PUT",
        &inbound,
        token,
        json!({"policy": "owner_agents"}),
    )
    .await;
    assert_eq!(
        (status, policy),
        (StatusCode::OK, json!({"policy": "owner_agents"}))
    );
    for bad in [
        json!({"policy": "everyone"}),
        json!({}),
        json!({"policy": "contacts", "chats": "allowed"}),
    ] {
        let (status, refused) = call(&api, "PUT", &inbound, token, bad.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {refused}");
        assert_eq!(refused["error"]["code"], "invalid", "{bad}");
    }
    // Invalid bodies never reach message-svc.
    assert_eq!(
        stub.calls(),
        [
            "settings".to_owned(),
            r#"set_outgoing [None, Some("allowed"), None, Some("ask")]"#.to_owned(),
            "set_outgoing [None, None, None, None]".to_owned(),
            "settings".to_owned(),
            "set_inbound owner_agents".to_owned(),
        ]
    );

    let events: Vec<Value> = super::security_events()
        .into_iter()
        .filter(|event| event["agent_id"] == agent.as_str())
        .collect();
    let named = |name: &str| -> Vec<Value> {
        events
            .iter()
            .filter(|event| event["event"] == name)
            .cloned()
            .collect()
    };
    assert_eq!(
        named("agent.outgoing_policy.read")[0]["policy"],
        "requests:ask chats:ask groups:off calls:off"
    );
    assert_eq!(
        named("agent.outgoing_policy.changed")[0]["policy"],
        "requests:ask chats:allowed groups:off calls:ask"
    );
    assert_eq!(
        named("agent.inbound_policy.read")[0]["policy"],
        "owner_only"
    );
    assert_eq!(
        named("agent.inbound_policy.changed")[0]["policy"],
        "owner_agents"
    );
    assert_eq!(named("agent.owner.refused").len(), 7);
}

#[tokio::test]
async fn the_owner_lists_approves_and_declines_the_agents_waiting_actions() {
    super::capture_logs();
    let created = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
    let (request, group, call_action) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let stub = Arc::new(Stub {
        failing: Some(group),
        ..Stub::default()
    });
    let (world, auth, _, token, agent) = owned("proactive", Arc::clone(&stub)).await;
    let friend = super::password_user(&auth, &world.pool, "friend", 1)
        .await
        .remove(0);
    let unseen = super::password_user(&auth, &world.pool, "unseen", 1)
        .await
        .remove(0);
    let api = with_owners_hiding(auth.clone(), Arc::clone(&stub), vec![unseen.user_id]);
    let waiting =
        |action_id, kind, target: Option<Uuid>, title: Option<&str>, members| OutgoingAction {
            action_id,
            kind,
            target,
            conversation: None,
            title: title.map(str::to_owned),
            members,
            state: "waiting",
            error: None,
            created_at: created,
            resolved_at: None,
        };
    *stub.outgoing.lock().unwrap() = vec![
        waiting(request, "request", Some(friend.user_id), None, Vec::new()),
        waiting(
            group,
            "group",
            None,
            Some("Secret plans"),
            vec![friend.user_id, unseen.user_id],
        ),
        waiting(call_action, "call", Some(friend.user_id), None, Vec::new()),
    ];
    let token = token.as_str();
    let base = format!("/v1/agents/{agent}/outgoing");

    let (status, listed) = call(&api, "GET", &base, token, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let actions = listed["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 3);
    assert_eq!(
        actions[0],
        json!({"action_id": request, "kind": "request", "target": actions[0]["target"],
               "conversation_id": null, "title": null, "members": [], "state": "waiting",
               "error": null, "created_at": "2026-09-21T14:13:20.000Z",
               "expires_at": "2026-09-22T14:13:20.000Z", "resolved_at": null})
    );
    assert_eq!(actions[0]["target"]["handle"], "friend");
    assert_eq!(actions[1]["kind"], "group");
    assert_eq!(actions[1]["target"], Value::Null);
    assert_eq!(actions[1]["title"], "Secret plans");
    assert_eq!(actions[1]["members"][0]["handle"], "friend");
    // Members the agent may not see are shown by id only.
    assert_eq!(actions[1]["members"][1]["state"], "hidden");
    assert_eq!(actions[1]["members"][1]["handle"], "");
    assert_eq!(actions[2]["kind"], "call");

    let (status, approved) = call(
        &api,
        "POST",
        &format!("{base}/{request}/approve"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["action"]["state"], "approved");
    assert_eq!(approved["action"]["error"], Value::Null);
    assert_eq!(
        approved["action"]["resolved_at"],
        "2026-09-21T14:18:20.000Z"
    );
    assert_eq!(approved["action"]["target"]["handle"], "friend");
    // The normal rules refused it: still 200, with the reason.
    let (status, failed) = call(
        &api,
        "POST",
        &format!("{base}/{group}/approve"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{failed}");
    assert_eq!(failed["action"]["state"], "failed");
    assert_eq!(failed["action"]["error"], "not_contact");
    let (status, declined) = call(
        &api,
        "POST",
        &format!("{base}/{call_action}/decline"),
        token,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{declined}");
    // Not waiting any more, unknown or not an id: 404.
    for path in [
        format!("{base}/{request}/approve"),
        format!("{base}/{call_action}/decline"),
        format!("{base}/{}/approve", Uuid::now_v7()),
        format!("{base}/not-an-id/decline"),
    ] {
        let (status, refused) = call(&api, "POST", &path, token, json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {refused}");
        assert_eq!(refused["error"]["code"], "not_found", "{path}");
    }
    let (status, listed) = call(&api, "GET", &base, token, json!({})).await;
    assert_eq!((status, listed), (StatusCode::OK, json!({"actions": []})));

    // Audited with ids, kinds and counts; never a title.
    let events: Vec<Value> = super::security_events()
        .into_iter()
        .filter(|event| event["agent_id"] == agent.as_str())
        .collect();
    let named = |name: &str| -> Vec<Value> {
        events
            .iter()
            .filter(|event| event["event"] == name)
            .cloned()
            .collect()
    };
    let listed = named("agent.outgoing.listed");
    assert_eq!(listed[0]["count"], 3);
    assert_eq!(listed[1]["count"], 0);
    let approved = named("agent.outgoing.approved");
    assert_eq!(approved[0]["action_id"], request.to_string());
    assert_eq!(approved[0]["kind"], "request");
    assert_eq!(approved[1]["action_id"], group.to_string());
    assert_eq!(approved[1]["kind"], "group");
    let declined = named("agent.outgoing.declined");
    assert_eq!(declined[0]["action_id"], call_action.to_string());
    assert_eq!(declined[0]["kind"], "call");
    let refused = named("agent.owner.refused");
    assert_eq!(refused.len(), 4);
    assert_eq!(refused[0]["action_id"], request.to_string());
    assert_eq!(refused[0]["status"], 404);
    assert!(!super::captured_logs().contains("Secret plans"));
}

#[tokio::test]
async fn the_new_owner_routes_are_unavailable_without_message_svc() {
    let (_world, auth, plain, owner) = setup().await;
    let (_, reservation) = call(
        &plain,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle":"offline","display_name":"Offline"}),
    )
    .await;
    let agent = reservation["user_id"].as_str().unwrap().to_owned();
    let api = router(Api {
        auth,
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: None,
        directory: Arc::new(AllBut(Vec::new())),
        templates: Arc::new(crate::templates::NoTemplates),
        call_history: Arc::new(crate::calls::NoCalls),
        agent_owners: Arc::new(crate::owners::NoAgentOwners),
        turn: Arc::new(crate::turn::Turn::disabled()),
    });
    let action = Uuid::now_v7().to_string();
    for (method, path, body) in routes(&agent, &action).into_iter().skip(11) {
        let (status, answer) = call(&api, method, &path, &owner.access_token, body).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path}: {answer}"
        );
        assert_eq!(answer["error"]["code"], "unavailable", "{method} {path}");
    }
}
