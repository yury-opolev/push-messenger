//! Proactive agents, whole stack (spec docs/superpowers/specs/2026-11-09-proactive-agents.md
//! §14): real auth-svc, worker, message-svc and gateway on PostgreSQL and Redis, no mocks.
//!
//! Two enrolled agents of one owner, Alpha and Beta, and a person who is Alpha's contact,
//! talk over raw sockets as their bridges would: the server's side of every bridge action
//! (the outgoing policy for requests, chats, groups and calls, the ask queue, the owner's
//! decisions, the inbound policy reaching the agent, `started_by_agent`, the audit). The
//! bridge's own side (Echo commands, held texts, the inbound policy and the loop guard
//! between two Echo bridges) is covered by push-bridge's tests (`core::actions_tests`,
//! `core::inbound_tests`).

mod common;

use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_auth_core::entra::testing::{KEY_A, StubOpenId, TENANT, sign};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    self, CallCancel, CallInvite, CallMedia, Capability, ContactState, ContentType, ErrorCode,
    Identify, InboundPolicy, OpenDirect, OutgoingAction, OutgoingKind, OutgoingPolicy,
    OutgoingState, Send, request, response,
};
use push_proto::{decode_frame, encode_frame, id_bytes};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A group title that must never reach the security log.
const GROUP_TITLE: &str = "plans-for-the-surprise-party";

const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
    m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n";

static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());
struct Capture;
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LOG.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A signed-in account: its user id and access token.
struct Account {
    user: Uuid,
    token: String,
}

async fn post(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    bearer: Option<&str>,
    body: Value,
) -> Value {
    let mut request = http.post(format!("{base}{path}")).json(&body);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}

/// An owner route: its status and JSON answer (`null` for an empty one).
async fn owner_call(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn device(http: &reqwest::Client, base: &str, registration: &str, name: &str) -> Account {
    let device = post(
        http,
        base,
        "/v1/devices",
        Some(registration),
        json!({"registration_id": Uuid::now_v7(), "name": name, "platform": "other"}),
    )
    .await;
    Account {
        user: device["user_id"].as_str().unwrap().parse().unwrap(),
        token: device["access_token"].as_str().unwrap().to_owned(),
    }
}

async fn person(http: &reqwest::Client, base: &str, stub: &StubOpenId, handle: &str) -> Account {
    let signed_in = post(
        http,
        base,
        "/v1/auth/entra",
        None,
        json!({"token": stub.token(&Uuid::new_v4().to_string()), "handle": handle}),
    )
    .await;
    device(
        http,
        base,
        signed_in["registration_token"].as_str().unwrap(),
        handle,
    )
    .await
}

async fn agent(
    http: &reqwest::Client,
    base: &str,
    stub: &StubOpenId,
    owner: &Account,
    handle: &str,
    name: &str,
) -> Account {
    let reservation = post(
        http,
        base,
        "/v1/agents",
        Some(&owner.token),
        json!({"handle": handle, "display_name": name}),
    )
    .await;
    let mut claims = stub.user_claims(TENANT, &Uuid::new_v4().to_string());
    claims.as_object_mut().unwrap().remove("scp");
    claims["idtyp"] = json!("app");
    let registration = post(
        http,
        base,
        "/v1/auth/entra-app",
        None,
        json!({"token": sign(KEY_A, &claims), "enrollment_code": reservation["enrollment_code"]}),
    )
    .await;
    device(
        http,
        base,
        registration["registration_token"].as_str().unwrap(),
        name,
    )
    .await
}

async fn send(socket: &mut Socket, body: Body) {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .unwrap();
}

async fn next(socket: &mut Socket) -> Body {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => return decode_frame(&bytes).unwrap(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("unexpected socket message {other:?}"),
            }
        }
    })
    .await
    .expect("socket frame within 15 seconds")
}

async fn ready(url: &str, token: &str) -> Socket {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (mut socket, _) = connect_async(url).await.unwrap();
            send(
                &mut socket,
                Body::Identify(Identify {
                    token: token.to_owned(),
                    capabilities: vec![Capability::Calls.into()],
                }),
            )
            .await;
            match socket.next().await.unwrap().unwrap() {
                Message::Binary(bytes)
                    if matches!(decode_frame(&bytes).unwrap(), Body::Ready(_)) =>
                {
                    return socket;
                }
                Message::Close(Some(frame)) if u16::from(frame.code) == 4006 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                other => panic!("identify failed {other:?}"),
            }
        }
    })
    .await
    .expect("worker relayed device within 30 seconds")
}

/// A request's answer, or its error.
async fn try_request(
    socket: &mut Socket,
    id: u64,
    body: request::Body,
) -> Result<response::Body, v1::Error> {
    send(
        socket,
        Body::Request(v1::Request {
            request_id: id,
            body: Some(body),
        }),
    )
    .await;
    loop {
        match next(socket).await {
            Body::Response(answer) if answer.request_id == id => return Ok(answer.body.unwrap()),
            Body::Error(error) if error.request_id == Some(id) => return Err(error),
            _ => {}
        }
    }
}

async fn request(socket: &mut Socket, id: u64, body: request::Body) -> response::Body {
    try_request(socket, id, body)
        .await
        .unwrap_or_else(|error| panic!("request {id} failed: {error:?}"))
}

/// The request's error, which must be `WAITING_FOR_OWNER`: the waiting action's id.
async fn waits(socket: &mut Socket, id: u64, body: request::Body) -> Uuid {
    let error = try_request(socket, id, body)
        .await
        .expect_err("the action waits for the owner");
    assert_eq!(error.code(), ErrorCode::WaitingForOwner, "{error:?}");
    Uuid::from_slice(error.action_id.as_deref().expect("the action's id")).unwrap()
}

fn ask(user: Uuid) -> request::Body {
    request::Body::ContactRequest(v1::ContactRequest {
        user_id: id_bytes(user),
    })
}

fn open_direct(peer: Uuid) -> request::Body {
    request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(peer),
    })
}

fn create_group(members: &[Uuid]) -> request::Body {
    request::Body::CreateGroup(v1::CreateGroup {
        title: GROUP_TITLE.to_owned(),
        member_ids: members.iter().map(|member| id_bytes(*member)).collect(),
    })
}

/// The next `OutgoingActionEvent` for `action`.
async fn action_event(socket: &mut Socket, action: Uuid) -> OutgoingAction {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Body::OutgoingActionEvent(event) = next(socket).await {
                let changed = event.action.expect("the action");
                if changed.action_id == id_bytes(action) {
                    break changed;
                }
            }
        }
    })
    .await
    .expect("the action's event")
}

/// The CallError for call.
async fn call_error(socket: &mut Socket, call: &[u8]) -> v1::CallError {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Body::CallError(error) = next(socket).await {
                if error.call_id == call {
                    break error;
                }
            }
        }
    })
    .await
    .expect("the call is refused")
}

async fn contacts(socket: &mut Socket, id: u64) -> Vec<(Vec<u8>, ContactState)> {
    let response::Body::Contacts(list) =
        request(socket, id, request::Body::ListContacts(v1::ListContacts {})).await
    else {
        panic!("a contact list");
    };
    list.contacts
        .into_iter()
        .map(|contact| {
            let state = contact.state();
            (contact.user_id, state)
        })
        .collect()
}

/// Sends `text` and waits for the recipient to get it.
async fn converse(sender: &mut Socket, recipient: &mut Socket, conversation: &[u8], text: &str) {
    let client_msg_id = id_bytes(Uuid::now_v7());
    send(
        sender,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.to_vec(),
            content_type: ContentType::Text.into(),
            body: text.as_bytes().to_vec(),
            client_ts: None,
        }),
    )
    .await;
    loop {
        if let Body::SendAck(ack) = next(sender).await {
            assert_eq!(ack.client_msg_id, client_msg_id);
            break;
        }
    }
    loop {
        if let Body::Deliver(entry) = next(recipient).await {
            if entry.client_msg_id == client_msg_id {
                assert_eq!(entry.body.as_deref(), Some(text.as_bytes()));
                break;
            }
        }
    }
}

/// The security log's events, as `(event, the whole line)`.
fn security_events() -> Vec<(String, Value)> {
    String::from_utf8(LOG.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["target"] == "security")
        .filter_map(|line| Some((line["fields"]["event"].as_str()?.to_owned(), line)))
        .collect()
}

#[tokio::test]
async fn agents_act_under_their_owners_policies() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(|| Capture)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    )
    .unwrap();
    let stub = StubOpenId::start().await;
    let (mut datastores, _containers) = common::calls_datastores().await;
    let message_url = datastores
        .message_svc
        .iter()
        .find(|(key, _)| key == "PUSH_DATABASE_URL")
        .unwrap()
        .1
        .clone();
    datastores.auth.extend(stub.variables());
    datastores
        .auth
        .push(("PUSH_AUTH_PROVIDERS".into(), "entra,entra-app".into()));
    // Groups are served here (the chart's gateway.groups), so the owner's groups policy is
    // what answers an agent's CreateGroup; with groups off the gateway answers
    // NOT_AVAILABLE first.
    datastores.gateway.push(("PUSH_GROUPS".into(), "1".into()));
    datastores.worker.extend([
        ("PUSH_EMAIL_SENDER".into(), "none".into()),
        ("PUSH_DEV_MAILBOX".into(), "0".into()),
    ]);
    let stack = common::Stack::start(datastores).await;
    let base = stack.auth_url.clone();
    let http = reqwest::Client::new();
    let message_db = sqlx::PgPool::connect(&message_url).await.unwrap();

    let owner = person(&http, &base, &stub, "alpha_owner").await;
    let friend = person(&http, &base, &stub, "agents_friend").await;
    let stranger = person(&http, &base, &stub, "a_stranger").await;
    let alpha = agent(&http, &base, &stub, &owner, "alpha_agent", "Alpha").await;
    let beta = agent(&http, &base, &stub, &owner, "beta_agent", "Beta").await;

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let enrolled: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM messaging.agent_owner WHERE owner_user_id = $1",
            )
            .bind(owner.user)
            .fetch_one(&message_db)
            .await
            .unwrap();
            if enrolled == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("both agents enrolled in message-svc");

    let url = stack.socket_url.clone();
    let mut owner_socket = ready(&url, &owner.token).await;
    let mut alpha_socket = ready(&url, &alpha.token).await;
    let mut beta_socket = ready(&url, &beta.token).await;
    let mut friend_socket = ready(&url, &friend.token).await;
    let alpha_base = format!("/v1/agents/{}", alpha.user);
    let beta_base = format!("/v1/agents/{}", beta.user);
    let get = |path: String| {
        let http = http.clone();
        let base = base.clone();
        let token = owner.token.clone();
        async move { owner_call(&http, &base, reqwest::Method::GET, &path, &token, None).await }
    };
    let put = |path: String, body: Value| {
        let http = http.clone();
        let base = base.clone();
        let token = owner.token.clone();
        async move {
            owner_call(
                &http,
                &base,
                reqwest::Method::PUT,
                &path,
                &token,
                Some(body),
            )
            .await
        }
    };
    let post_owner = |path: String| {
        let http = http.clone();
        let base = base.clone();
        let token = owner.token.clone();
        async move { owner_call(&http, &base, reqwest::Method::POST, &path, &token, None).await }
    };

    // The defaults.
    assert_eq!(
        get(format!("{alpha_base}/outgoing-policy")).await,
        (
            200,
            json!({"requests": "ask", "chats": "ask", "groups": "off", "calls": "off"})
        )
    );

    // 1. Alpha's contact request to Beta waits for the owner (requests: ask).
    let request_action = waits(&mut alpha_socket, 1, ask(beta.user)).await;
    let told = action_event(&mut owner_socket, request_action).await;
    assert_eq!(told.state(), OutgoingState::Waiting);
    assert_eq!(told.kind(), OutgoingKind::Request);
    assert_eq!(told.agent_user_id, id_bytes(alpha.user));
    assert_eq!(told.target_user_id, id_bytes(beta.user));
    // Asked again while it waits: the same action.
    assert_eq!(
        waits(&mut alpha_socket, 2, ask(beta.user)).await,
        request_action
    );
    let (status, listed) = get(format!("{alpha_base}/outgoing")).await;
    assert_eq!(status, 200, "{listed}");
    let actions = listed["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 1, "{listed}");
    assert_eq!(actions[0]["action_id"], request_action.to_string());
    assert_eq!(actions[0]["kind"], "request");
    assert_eq!(actions[0]["state"], "waiting");
    // Alpha cannot see Beta yet, so the owner sees the target by id only.
    assert_eq!(actions[0]["target"]["user_id"], beta.user.to_string());
    assert_eq!(actions[0]["target"]["state"], "hidden");
    let (status, approved) =
        post_owner(format!("{alpha_base}/outgoing/{request_action}/approve")).await;
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved["action"]["state"], "approved", "{approved}");
    let outcome = action_event(&mut alpha_socket, request_action).await;
    assert_eq!(outcome.state(), OutgoingState::Approved);
    // Beta's connection policy (ask) holds the request; the owner approves it too.
    let (status, waiting) = get(format!("{beta_base}/requests")).await;
    assert_eq!(status, 200, "{waiting}");
    assert_eq!(waiting["requests"][0]["user"]["handle"], "alpha_agent");
    let (status, _) = post_owner(format!("{beta_base}/requests/{}/approve", alpha.user)).await;
    assert_eq!(status, 204);
    assert!(
        contacts(&mut alpha_socket, 3)
            .await
            .contains(&(id_bytes(beta.user), ContactState::Accepted))
    );

    // 2. Alpha opens a chat with Beta: it waits (chats: ask); approved, the server opens it
    // and Alpha hears where.
    let chat_action = waits(&mut alpha_socket, 4, open_direct(beta.user)).await;
    let told = action_event(&mut owner_socket, chat_action).await;
    assert_eq!(
        (told.state(), told.kind()),
        (OutgoingState::Waiting, OutgoingKind::Chat)
    );
    let (status, approved) =
        post_owner(format!("{alpha_base}/outgoing/{chat_action}/approve")).await;
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved["action"]["state"], "approved", "{approved}");
    let outcome = action_event(&mut alpha_socket, chat_action).await;
    assert_eq!(outcome.state(), OutgoingState::Approved);
    let chat = outcome.conversation_id.clone();
    let chat_id = Uuid::from_slice(&chat).unwrap();
    assert_eq!(approved["action"]["conversation_id"], chat_id.to_string());
    // The chat exists now: opening it again is no action, and Alpha's text reaches Beta.
    let response::Body::DirectOpened(opened) =
        request(&mut alpha_socket, 5, open_direct(beta.user)).await
    else {
        panic!("a direct conversation");
    };
    assert_eq!(opened.conversation_id, chat);
    converse(&mut alpha_socket, &mut beta_socket, &chat, "hello").await;
    let (status, view) = get(format!("{alpha_base}/conversations")).await;
    assert_eq!(status, 200, "{view}");
    let listed = view["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|conversation| conversation["conversation_id"] == chat_id.to_string())
        .expect("the chat is in the owner view")
        .clone();
    assert_eq!(listed["started_by_agent"], true, "{listed}");
    let response::Body::Conversations(list) = request(
        &mut alpha_socket,
        6,
        request::Body::ListConversations(v1::ListConversations::default()),
    )
    .await
    else {
        panic!("a conversation list");
    };
    let summary = list
        .conversations
        .iter()
        .find(|conversation| conversation.conversation_id == chat)
        .expect("Alpha lists the chat");
    assert!(summary.started_by_agent);

    // 3. The inbound policy: owner_only by default; owner_agents for both agents, which
    // they read and are told of.
    for agent_base in [&alpha_base, &beta_base] {
        assert_eq!(
            get(format!("{agent_base}/inbound-policy")).await,
            (200, json!({"policy": "owner_only"}))
        );
        assert_eq!(
            put(
                format!("{agent_base}/inbound-policy"),
                json!({"policy": "owner_agents"})
            )
            .await,
            (200, json!({"policy": "owner_agents"}))
        );
    }
    let told = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Body::AgentSettingsEvent(event) = next(&mut beta_socket).await {
                break event.settings.expect("the settings");
            }
        }
    })
    .await
    .expect("Beta hears of its settings");
    assert_eq!(told.inbound(), InboundPolicy::OwnerAgents);
    let response::Body::AgentSettings(settings) = request(
        &mut beta_socket,
        7,
        request::Body::GetAgentSettings(v1::GetAgentSettings {}),
    )
    .await
    else {
        panic!("Beta's settings");
    };
    assert_eq!(settings.inbound(), InboundPolicy::OwnerAgents);
    assert_eq!(settings.requests(), OutgoingPolicy::Ask);
    assert_eq!(settings.groups(), OutgoingPolicy::Off);
    // A person has no agent settings.
    let refused = try_request(
        &mut friend_socket,
        8,
        request::Body::GetAgentSettings(v1::GetAgentSettings {}),
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::NotAllowed);

    // 4. With chats off a new chat is refused at once; groups are off by default. The
    // friend becomes Alpha's contact first (the owner approves the request).
    assert_eq!(
        put(
            format!("{alpha_base}/outgoing-policy"),
            json!({"chats": "off"})
        )
        .await,
        (
            200,
            json!({"requests": "ask", "chats": "off", "groups": "off", "calls": "off"})
        )
    );
    let response::Body::Contact(contact) = request(&mut friend_socket, 9, ask(alpha.user)).await
    else {
        panic!("a contact answer");
    };
    assert_eq!(contact.state(), ContactState::RequestedOut);
    let (status, _) = post_owner(format!("{alpha_base}/requests/{}/approve", friend.user)).await;
    assert_eq!(status, 204);
    let refused = try_request(&mut alpha_socket, 10, open_direct(friend.user))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::NotAllowedByOwner, "{refused:?}");
    assert_eq!(refused.action_id, None);
    let refused = try_request(&mut alpha_socket, 11, create_group(&[friend.user]))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::NotAllowedByOwner, "{refused:?}");
    // A reply in the chat that exists is never an action.
    converse(&mut alpha_socket, &mut beta_socket, &chat, "still here").await;

    // 5. A declined action (groups: ask) reaches Alpha as declined by the owner.
    let (status, _) = put(
        format!("{alpha_base}/outgoing-policy"),
        json!({"groups": "ask"}),
    )
    .await;
    assert_eq!(status, 200);
    let group_action = waits(&mut alpha_socket, 12, create_group(&[friend.user])).await;
    let told = action_event(&mut owner_socket, group_action).await;
    assert_eq!(
        (told.state(), told.kind(), told.title.as_str()),
        (OutgoingState::Waiting, OutgoingKind::Group, GROUP_TITLE)
    );

    // 6. Nobody but the owner sees or decides any of it: the stranger, the friend, both
    // agents (Alpha itself included) get not_found on every new route.
    let routes = [
        (reqwest::Method::GET, "outgoing-policy".to_owned()),
        (reqwest::Method::PUT, "outgoing-policy".to_owned()),
        (reqwest::Method::GET, "inbound-policy".to_owned()),
        (reqwest::Method::PUT, "inbound-policy".to_owned()),
        (reqwest::Method::GET, "outgoing".to_owned()),
        (
            reqwest::Method::POST,
            format!("outgoing/{group_action}/approve"),
        ),
        (
            reqwest::Method::POST,
            format!("outgoing/{group_action}/decline"),
        ),
    ];
    for caller in [&stranger, &friend, &alpha, &beta] {
        for (method, route) in &routes {
            let body = (*method == reqwest::Method::PUT).then(|| json!({"policy": "contacts"}));
            let path = format!("{alpha_base}/{route}");
            let (status, answer) =
                owner_call(&http, &base, method.clone(), &path, &caller.token, body).await;
            assert_eq!(status, 404, "{method} {path}: {answer}");
            assert_eq!(answer["error"]["code"], "not_found", "{method} {path}");
        }
    }
    let (status, listed) = get(format!("{alpha_base}/outgoing")).await;
    assert_eq!(status, 200);
    assert_eq!(
        listed["actions"][0]["action_id"],
        group_action.to_string(),
        "the action still waits: {listed}"
    );
    assert_eq!(listed["actions"][0]["title"], GROUP_TITLE);

    let (status, declined) =
        post_owner(format!("{alpha_base}/outgoing/{group_action}/decline")).await;
    assert_eq!((status, declined), (204, Value::Null));
    let outcome = action_event(&mut alpha_socket, group_action).await;
    assert_eq!(outcome.state(), OutgoingState::OwnerDeclined);
    let told = action_event(&mut owner_socket, group_action).await;
    assert_eq!(told.state(), OutgoingState::OwnerDeclined);
    assert_eq!(
        post_owner(format!("{alpha_base}/outgoing/{group_action}/decline"))
            .await
            .0,
        404,
        "decided already"
    );
    assert_eq!(
        get(format!("{alpha_base}/outgoing")).await,
        (200, json!({"actions": []}))
    );
    let response::Body::OutgoingActions(own) = request(
        &mut alpha_socket,
        13,
        request::Body::ListOutgoingActions(v1::ListOutgoingActions {}),
    )
    .await
    else {
        panic!("Alpha's actions");
    };
    let states: Vec<(Vec<u8>, OutgoingState)> = own
        .actions
        .iter()
        .map(|action| (action.action_id.clone(), action.state()))
        .collect();
    assert_eq!(
        states,
        [
            (id_bytes(request_action), OutgoingState::Approved),
            (id_bytes(chat_action), OutgoingState::Approved),
            (id_bytes(group_action), OutgoingState::OwnerDeclined),
        ]
    );

    // 7. Calls to a contact (calls: off by default, then ask): an approved call rings that
    // person once, and both histories mark it started_by_agent.
    let invite = |call_id: &[u8]| {
        Body::CallInvite(CallInvite {
            call_id: call_id.to_vec(),
            callee_user_id: id_bytes(friend.user),
            offer_sdp: OFFER.to_owned(),
            media: vec![CallMedia::Audio.into()],
        })
    };
    let call = id_bytes(Uuid::now_v7());
    send(&mut alpha_socket, invite(&call)).await;
    let refused = call_error(&mut alpha_socket, &call).await;
    assert_eq!(refused.code(), ErrorCode::NotAllowedByOwner, "{refused:?}");
    let (status, _) = put(
        format!("{alpha_base}/outgoing-policy"),
        json!({"calls": "ask"}),
    )
    .await;
    assert_eq!(status, 200);
    let call = id_bytes(Uuid::now_v7());
    send(&mut alpha_socket, invite(&call)).await;
    let refused = call_error(&mut alpha_socket, &call).await;
    assert_eq!(refused.code(), ErrorCode::WaitingForOwner, "{refused:?}");
    let call_action =
        Uuid::from_slice(refused.action_id.as_deref().expect("the action's id")).unwrap();
    let told = action_event(&mut owner_socket, call_action).await;
    assert_eq!(
        (told.state(), told.kind()),
        (OutgoingState::Waiting, OutgoingKind::Call)
    );
    let (status, approved) =
        post_owner(format!("{alpha_base}/outgoing/{call_action}/approve")).await;
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved["action"]["state"], "approved", "{approved}");
    assert_eq!(
        action_event(&mut alpha_socket, call_action).await.state(),
        OutgoingState::Approved
    );
    let call = id_bytes(Uuid::now_v7());
    send(&mut alpha_socket, invite(&call)).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Body::CallIncoming(incoming) = next(&mut friend_socket).await {
                if incoming.call_id == call {
                    assert_eq!(incoming.caller_user_id, id_bytes(alpha.user));
                    break;
                }
            }
        }
    })
    .await
    .expect("the approved call rings");
    send(
        &mut alpha_socket,
        Body::CallCancel(CallCancel {
            call_id: call.clone(),
        }),
    )
    .await;
    let call_text = Uuid::from_slice(&call).unwrap().to_string();
    let history = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let (status, page) = owner_call(
                &http,
                &base,
                reqwest::Method::GET,
                "/v1/calls",
                &friend.token,
                None,
            )
            .await;
            assert_eq!(status, 200, "{page}");
            if let Some(entry) = page["calls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["call_id"] == call_text)
            {
                break entry.clone();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the call is in the friend's history");
    assert_eq!(history["started_by_agent"], true, "{history}");
    assert_eq!(history["direction"], "in");
    let (status, calls) = get(format!("{alpha_base}/calls")).await;
    assert_eq!(status, 200, "{calls}");
    let entry = calls["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["call_id"] == call_text)
        .expect("the call is in the owner view")
        .clone();
    assert_eq!(entry["started_by_agent"], true, "{entry}");
    // The approval was used: the next call waits again.
    let call = id_bytes(Uuid::now_v7());
    send(&mut alpha_socket, invite(&call)).await;
    let refused = call_error(&mut alpha_socket, &call).await;
    assert_eq!(refused.code(), ErrorCode::WaitingForOwner, "{refused:?}");
    assert_ne!(
        refused.action_id.as_deref(),
        Some(&id_bytes(call_action)[..])
    );

    // 8. The security log has the owner's changes and decisions, without content.
    let events = security_events();
    for (event, action) in [
        ("agent.outgoing_policy.changed", None),
        ("agent.inbound_policy.changed", None),
        ("agent.outgoing.listed", None),
        ("agent.outgoing.approved", Some(request_action)),
        ("agent.outgoing.approved", Some(chat_action)),
        ("agent.outgoing.declined", Some(group_action)),
    ] {
        let line = events
            .iter()
            .find(|(name, line)| {
                name == event
                    && line["fields"]["status"].as_u64().is_some_and(|s| s < 300)
                    && action.is_none_or(|action| line["fields"]["action_id"] == action.to_string())
            })
            .unwrap_or_else(|| panic!("{event} {action:?} is logged"));
        assert_eq!(line.1["fields"]["agent_id"], alpha.user.to_string());
        assert_eq!(line.1["fields"]["user_id"], owner.user.to_string());
    }
    let declined = events
        .iter()
        .find(|(name, _)| name == "agent.outgoing.declined")
        .unwrap();
    assert_eq!(declined.1["fields"]["kind"], "group");
    let changed = events
        .iter()
        .filter(|(name, _)| name == "agent.outgoing_policy.changed")
        .map(|(_, line)| line["fields"]["policy"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        changed.contains(&"requests:ask chats:off groups:off calls:off".to_owned()),
        "{changed:?}"
    );
    assert!(
        events
            .iter()
            .any(|(name, line)| name == "agent.owner.refused" && line["fields"]["status"] == 404),
        "refused owner calls are logged"
    );
    for (_, line) in &events {
        let text = line.to_string();
        for content in [GROUP_TITLE, "hello", "still here"] {
            assert!(
                !text.contains(content),
                "content in the security log: {text}"
            );
        }
    }

    for socket in [
        &mut owner_socket,
        &mut alpha_socket,
        &mut beta_socket,
        &mut friend_socket,
    ] {
        let _ = socket.close(None).await;
    }
    message_db.close().await;
    stack.stop().await;
}
