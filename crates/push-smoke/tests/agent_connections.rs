//! Agent connections and the owner view, whole stack (spec
//! docs/superpowers/specs/2026-11-06-agent-connections-and-owner-view.md §11): real
//! auth-svc, worker, message-svc and gateway on PostgreSQL and Redis, no mocks.
//!
//! Coda itself is an external program, so two enrolled agents of the same owner stand in:
//! "echo" (the echo backend's role) and "coda" (Coda's), each talking over a raw socket.

mod common;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_auth_core::entra::testing::{KEY_A, StubOpenId, TENANT, sign};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    self, CallInvite, CallMedia, Capability, ContactState, ContentType, ErrorCode, Identify,
    OpenDirect, Send, request, response,
};
use push_proto::{decode_frame, encode_frame, id_bytes};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
    m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n";

/// A signed-in account: its user id, device id and access token.
struct Account {
    user: Uuid,
    device: Uuid,
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
        device: device["device_id"].as_str().unwrap().parse().unwrap(),
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

/// A frame if one comes within `wait`.
async fn maybe_next(socket: &mut Socket, wait: Duration) -> Option<Body> {
    tokio::time::timeout(wait, next(socket)).await.ok()
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

fn ask(user: Uuid) -> request::Body {
    request::Body::ContactRequest(v1::ContactRequest {
        user_id: id_bytes(user),
    })
}

fn requested(answer: response::Body) -> ContactState {
    let response::Body::Contact(contact) = answer else {
        panic!("a contact answer, got {answer:?}");
    };
    contact.state()
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

/// Every owner route of `agent`, as `(method, path)`.
fn owner_routes(agent: Uuid, user: Uuid, conversation: &str) -> Vec<(reqwest::Method, String)> {
    let base = format!("/v1/agents/{agent}");
    vec![
        (reqwest::Method::GET, format!("{base}/policy")),
        (reqwest::Method::PUT, format!("{base}/policy")),
        (reqwest::Method::GET, format!("{base}/requests")),
        (
            reqwest::Method::POST,
            format!("{base}/requests/{user}/approve"),
        ),
        (
            reqwest::Method::POST,
            format!("{base}/requests/{user}/decline"),
        ),
        (
            reqwest::Method::POST,
            format!("{base}/requests/{user}/block"),
        ),
        (reqwest::Method::GET, format!("{base}/contacts")),
        (reqwest::Method::DELETE, format!("{base}/contacts/{user}")),
        (reqwest::Method::GET, format!("{base}/conversations")),
        (
            reqwest::Method::GET,
            format!("{base}/conversations/{conversation}/messages"),
        ),
        (reqwest::Method::GET, format!("{base}/calls")),
    ]
}

#[tokio::test]
async fn agents_connect_under_their_owners_policy_and_the_owner_reads_their_chats() {
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
    // Refusals end within a second or two here (in production: 10 minutes to 6 hours).
    datastores.message_svc.extend([
        ("PUSH_AGENT_REFUSAL_MIN_S".into(), "0".into()),
        ("PUSH_AGENT_REFUSAL_MAX_S".into(), "1".into()),
        ("PUSH_AGENT_SWEEP_SECONDS".into(), "1".into()),
    ]);
    datastores.worker.extend([
        ("PUSH_EMAIL_SENDER".into(), "none".into()),
        ("PUSH_DEV_MAILBOX".into(), "0".into()),
    ]);
    let stack = common::Stack::start(datastores).await;
    let base = stack.auth_url.clone();
    let http = reqwest::Client::new();
    let message_db = sqlx::PgPool::connect(&message_url).await.unwrap();

    let owner = person(&http, &base, &stub, "coda_owner").await;
    let friend = person(&http, &base, &stub, "owners_friend").await;
    let stranger = person(&http, &base, &stub, "a_stranger").await;
    let echo = agent(&http, &base, &stub, &owner, "echo_agent", "Echo").await;
    let coda = agent(&http, &base, &stub, &owner, "coda_agent", "Coda").await;

    // The worker relays both enrollments: each agent and its owner are contacts.
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
    // An agent's contact requests and new chats wait for its owner by default (spec
    // 2026-11-09 proactive agents); this owner lets Echo's go at once.
    sqlx::query(
        "INSERT INTO messaging.agent_settings \
         (agent_user_id, inbound, requests, chats, groups, calls, changed_at) \
         VALUES ($1, 1, 1, 1, 3, 3, now())",
    )
    .bind(echo.user)
    .execute(&message_db)
    .await
    .unwrap();

    let url = stack.socket_url.clone();
    let mut owner_socket = ready(&url, &owner.token).await;
    let mut echo_socket = ready(&url, &echo.token).await;
    let mut coda_socket = ready(&url, &coda.token).await;
    let mut friend_socket = ready(&url, &friend.token).await;
    let mut stranger_socket = ready(&url, &stranger.token).await;
    let coda_base = format!("/v1/agents/{}", coda.user);

    // 1. Echo asks Coda (policy ask): the request waits for the owner, who is told.
    let (status, policy) = owner_call(
        &http,
        &base,
        reqwest::Method::GET,
        &format!("{coda_base}/policy"),
        &owner.token,
        None,
    )
    .await;
    assert_eq!((status, policy), (200, json!({"policy": "ask"})));
    assert_eq!(
        requested(request(&mut echo_socket, 1, ask(coda.user)).await),
        ContactState::RequestedOut
    );
    let waiting = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Body::AgentContactEvent(event) = next(&mut owner_socket).await {
                if event.agent_user_id == id_bytes(coda.user) {
                    break event;
                }
            }
        }
    })
    .await
    .expect("the owner hears of the waiting request");
    assert_eq!(waiting.user_id, id_bytes(echo.user));
    assert_eq!(waiting.state(), ContactState::RequestedIn);
    let (status, requests) = owner_call(
        &http,
        &base,
        reqwest::Method::GET,
        &format!("{coda_base}/requests"),
        &owner.token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{requests}");
    assert_eq!(requests["requests"][0]["user"]["handle"], "echo_agent");
    assert_eq!(
        requests["requests"][0]["user"]["owner_handle"],
        "coda_owner"
    );
    // Coda cannot decide for itself.
    let refused = try_request(
        &mut coda_socket,
        2,
        request::Body::ContactAccept(v1::ContactAccept {
            user_id: id_bytes(echo.user),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::OwnerDecides);
    let (status, _) = owner_call(
        &http,
        &base,
        reqwest::Method::POST,
        &format!("{coda_base}/requests/{}/approve", echo.user),
        &owner.token,
        None,
    )
    .await;
    assert_eq!(status, 204);

    // They exchange messages.
    let response::Body::DirectOpened(chat) = request(
        &mut echo_socket,
        3,
        request::Body::OpenDirect(OpenDirect {
            peer_user_id: id_bytes(coda.user),
        }),
    )
    .await
    else {
        panic!("a direct conversation");
    };
    converse(
        &mut echo_socket,
        &mut coda_socket,
        &chat.conversation_id,
        "hello Coda, echo here",
    )
    .await;
    converse(
        &mut coda_socket,
        &mut echo_socket,
        &chat.conversation_id,
        "hello Echo",
    )
    .await;
    let chat_id = Uuid::from_slice(&chat.conversation_id).unwrap();

    // The owner reads that chat in Coda's view; Coda's read state does not move.
    let read_state = || async {
        let read: i64 = sqlx::query_scalar(
            "SELECT read_seq FROM messaging.conversation_member \
             WHERE conversation_id = $1 AND user_id = $2",
        )
        .bind(chat_id)
        .bind(coda.user)
        .fetch_one(&message_db)
        .await
        .unwrap();
        let delivered: Option<i64> = sqlx::query_scalar(
            "SELECT delivered_seq FROM messaging.device_cursor \
             WHERE device_id = $1 AND conversation_id = $2",
        )
        .bind(coda.device)
        .bind(chat_id)
        .fetch_optional(&message_db)
        .await
        .unwrap();
        (read, delivered)
    };
    let before = read_state().await;
    let (status, listed) = owner_call(
        &http,
        &base,
        reqwest::Method::GET,
        &format!("{coda_base}/conversations"),
        &owner.token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    let conversation = listed["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|conversation| conversation["conversation_id"] == chat_id.to_string())
        .expect("the chat with Echo is listed")
        .clone();
    assert_eq!(conversation["kind"], "direct");
    assert_eq!(conversation["peer"]["handle"], "echo_agent");
    assert_eq!(conversation["peer_owner"]["handle"], "coda_owner");
    assert_eq!(conversation["message_count"], 2);
    let (status, page) = owner_call(
        &http,
        &base,
        reqwest::Method::GET,
        &format!("{coda_base}/conversations/{chat_id}/messages"),
        &owner.token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{page}");
    let bodies: Vec<&str> = page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, ["hello Coda, echo here", "hello Echo"]);
    let (status, calls) = owner_call(
        &http,
        &base,
        reqwest::Method::GET,
        &format!("{coda_base}/calls"),
        &owner.token,
        None,
    )
    .await;
    assert_eq!((status, calls), (200, json!({"calls": []})));
    assert_eq!(
        read_state().await,
        before,
        "the owner's reads change nothing"
    );
    // There is no way to write as the agent.
    let (status, _) = owner_call(
        &http,
        &base,
        reqwest::Method::POST,
        &format!("{coda_base}/conversations/{chat_id}/messages"),
        &owner.token,
        Some(json!({"body": "as the agent"})),
    )
    .await;
    assert_eq!(status, 405);

    // 2. auto_owner_contacts: the owner's accepted contact is accepted with no prompt.
    assert_eq!(
        requested(request(&mut owner_socket, 10, ask(friend.user)).await),
        ContactState::RequestedOut
    );
    request(
        &mut friend_socket,
        11,
        request::Body::ContactAccept(v1::ContactAccept {
            user_id: id_bytes(owner.user),
        }),
    )
    .await;
    let (status, _) = owner_call(
        &http,
        &base,
        reqwest::Method::PUT,
        &format!("{coda_base}/policy"),
        &owner.token,
        Some(json!({"policy": "auto_owner_contacts"})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        requested(request(&mut friend_socket, 12, ask(coda.user)).await),
        ContactState::Accepted
    );

    // The notice data: whoever sees Coda sees its owner.
    for viewer in [&friend, &echo] {
        let profiles: Value = http
            .get(format!("{base}/v1/users?ids={}", coda.user))
            .bearer_auth(&viewer.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let profile = &profiles["users"][0];
        assert_eq!(profile["kind"], "agent");
        assert_eq!(profile["owner_user_id"], owner.user.to_string());
        assert_eq!(profile["owner_handle"], "coda_owner");
        assert!(profile["owner_display_name"].is_string(), "{profile}");
    }

    // 3. refuse_all: the stranger sees an ordinary pending request that ends like a
    // decline, without any event.
    let (status, _) = owner_call(
        &http,
        &base,
        reqwest::Method::PUT,
        &format!("{coda_base}/policy"),
        &owner.token,
        Some(json!({"policy": "refuse_all"})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        requested(request(&mut stranger_socket, 20, ask(coda.user)).await),
        ContactState::RequestedOut
    );
    let mut request_id = 21;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            request_id += 1;
            let listed = contacts(&mut stranger_socket, request_id).await;
            if !listed.iter().any(|(user, _)| *user == id_bytes(coda.user)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the refused request ends");
    while let Some(frame) = maybe_next(&mut stranger_socket, Duration::from_millis(500)).await {
        if let Body::ContactEvent(event) = frame {
            assert_ne!(
                (event.state(), event.user_id),
                (ContactState::None, id_bytes(coda.user)),
                "a refusal is never announced"
            );
        }
    }
    let (status, waiting) = owner_call(
        &http,
        &base,
        reqwest::Method::GET,
        &format!("{coda_base}/requests"),
        &owner.token,
        None,
    )
    .await;
    assert_eq!((status, waiting), (200, json!({"requests": []})));

    // 4. Everyone but the owner gets not_found on every owner route.
    let chat_text = chat_id.to_string();
    for caller in [&stranger, &friend, &coda, &echo] {
        for (method, path) in owner_routes(coda.user, stranger.user, &chat_text) {
            let body = (method == reqwest::Method::PUT).then(|| json!({"policy": "auto_any"}));
            let (status, answer) =
                owner_call(&http, &base, method.clone(), &path, &caller.token, body).await;
            assert_eq!(status, 404, "{method} {path}: {answer}");
            assert_eq!(answer["error"]["code"], "not_found", "{method} {path}");
        }
    }

    // 5. Agents never call each other.
    let call_id = id_bytes(Uuid::now_v7());
    send(
        &mut echo_socket,
        Body::CallInvite(CallInvite {
            call_id: call_id.clone(),
            callee_user_id: id_bytes(coda.user),
            offer_sdp: OFFER.to_owned(),
            media: vec![CallMedia::Audio.into()],
        }),
    )
    .await;
    let refused = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Body::CallError(error) = next(&mut echo_socket).await {
                if error.call_id == call_id {
                    break error;
                }
            }
        }
    })
    .await
    .expect("the call is refused");
    assert_eq!(refused.code(), ErrorCode::NotAllowed);

    for socket in [
        &mut owner_socket,
        &mut echo_socket,
        &mut coda_socket,
        &mut friend_socket,
        &mut stranger_socket,
    ] {
        let _ = socket.close(None).await;
    }
    message_db.close().await;
    stack.stop().await;
}
