//! Rich message templates through the whole stack (spec 2026-10-26): an owned agent's card
//! and its owner's answer over the gateway, a template submitted on auth-svc, reviewed on
//! its admin listener and served by the catalog, and message-svc enforcing each step.
//! Real auth/worker/message-svc/gateway with PostgreSQL and Redis; no cloud or mocks.

mod common;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_auth_core::entra::testing::{KEY_A, StubOpenId, TENANT, sign};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, ContentType, ErrorCode, Identify, OpenDirect, Send, request, response};
use push_proto::{decode_frame, encode_frame, id_bytes};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const APPROVAL: &str = r#"{"tpl":"agent.approval@1","params":{"title":"Deploy v2","details":"Ship it to production?"},"fallback":"Deploy v2 to production?"}"#;
const NOTICE: &str =
    r#"{"tpl":"acme.notice@1","params":{"text":"The office is closed today"},"fallback":"Closed"}"#;
const BUILT_INS: [&str; 7] = [
    "agent.reply@1",
    "agent.approval@1",
    "agent.question@1",
    "agent.plan@1",
    "agent.choose@1",
    "agent.plan-approval@1",
    "agent.reply@2",
];

fn notice(version: u32) -> Value {
    json!({
        "format": "tpl/1",
        "publisher": "acme",
        "name": "notice",
        "version": version,
        "title": "Notice",
        "senders": ["human", "agent"],
        "params": {"text": {"type": "string", "max_len": 200}},
        "layout": [{"type": "text", "param": "text"}],
    })
}

/// One JSON request: its status and JSON body (`Null` when there is none).
async fn call(request: reqwest::RequestBuilder) -> (StatusCode, Value) {
    let response = request.send().await.unwrap();
    let status = response.status();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn post_ok(http: &reqwest::Client, url: String, bearer: Option<&str>, body: Value) -> Value {
    let mut request = http.post(&url).json(&body);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let (status, body) = call(request).await;
    assert!(status.is_success(), "{url}: {status} {body}");
    body
}

async fn submit(
    http: &reqwest::Client,
    stack: &common::Stack,
    token: &str,
    definition: &Value,
) -> (StatusCode, Value) {
    call(
        http.post(format!("{}/v1/templates", stack.auth_url))
            .bearer_auth(token)
            .json(definition),
    )
    .await
}

async fn review(http: &reqwest::Client, stack: &common::Stack, template: &str, action: &str) {
    let admin_url = stack
        .admin_url
        .as_deref()
        .expect("the stack has an admin listener");
    let (status, body) = call(
        http.post(format!(
            "{admin_url}/v1/admin/templates/{template}/{action}"
        ))
        .bearer_auth(stack.admin_token)
        .json(&json!({"reviewer": "smoke-reviewer", "note": "whole-stack test"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{action} {template}: {body}");
    let expected = if action == "approve" {
        "approved"
    } else {
        "revoked"
    };
    assert_eq!(body["status"], expected, "{body}");
}

/// The catalog after `since`: its body and ETag.
async fn catalog(
    http: &reqwest::Client,
    stack: &common::Stack,
    token: &str,
    since: i64,
) -> (Value, String) {
    let response = http
        .get(format!("{}/v1/templates?since={since}", stack.auth_url))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    (response.json().await.unwrap(), etag)
}

fn entry<'a>(catalog: &'a Value, template: &str) -> &'a Value {
    catalog["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["template"] == template)
        .unwrap_or_else(|| panic!("{template} missing from {catalog}"))
}

async fn send_frame(socket: &mut Socket, body: Body) {
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

/// A socket identified with `token`, retried while the worker has not relayed the device.
async fn ready(url: &str, token: &str) -> Socket {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (mut socket, _) = connect_async(url).await.unwrap();
            send_frame(
                &mut socket,
                Body::Identify(Identify {
                    token: token.to_owned(),
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

async fn request(socket: &mut Socket, id: u64, body: request::Body) -> response::Body {
    send_frame(
        socket,
        Body::Request(v1::Request {
            request_id: id,
            body: Some(body),
        }),
    )
    .await;
    loop {
        match next(socket).await {
            Body::Response(answer) if answer.request_id == id => return answer.body.unwrap(),
            Body::Error(error) => panic!("request failed {error:?}"),
            _ => {}
        }
    }
}

/// Sends one message and waits for its SendAck or its Error.
async fn send_message(
    socket: &mut Socket,
    conversation: &[u8],
    content_type: ContentType,
    body: &str,
) -> Result<v1::SendAck, v1::Error> {
    let client_msg_id = id_bytes(Uuid::now_v7());
    send_frame(
        socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.to_vec(),
            content_type: content_type.into(),
            body: body.as_bytes().to_vec(),
            client_ts: None,
        }),
    )
    .await;
    loop {
        match next(socket).await {
            Body::SendAck(ack) if ack.client_msg_id == client_msg_id => return Ok(ack),
            Body::Error(error) if error.client_msg_id.as_ref() == Some(&client_msg_id) => {
                return Err(error);
            }
            _ => {}
        }
    }
}

async fn accepted(
    socket: &mut Socket,
    conversation: &[u8],
    kind: ContentType,
    body: &str,
) -> v1::SendAck {
    send_message(socket, conversation, kind, body)
        .await
        .unwrap_or_else(|error| panic!("{body} refused: {error:?}"))
}

async fn refused(
    socket: &mut Socket,
    conversation: &[u8],
    kind: ContentType,
    body: &str,
    code: ErrorCode,
) {
    match send_message(socket, conversation, kind, body).await {
        Err(error) => assert_eq!(error.code(), code, "{body}: {error:?}"),
        Ok(ack) => panic!("{body} accepted: {ack:?}"),
    }
}

fn answer(card: &v1::SendAck, value: &str) -> String {
    let card = Uuid::from_slice(&card.message_id).unwrap().hyphenated();
    json!({"card": card.to_string(), "values": [value], "fallback": "Approved"}).to_string()
}

#[tokio::test]
async fn rich_templates_through_the_whole_stack() {
    let stub = StubOpenId::start().await;
    let (mut datastores, _containers) = common::password_datastores().await;
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
    // The admin listener is accepting connections once the stack has started.
    let stack = common::Stack::start_with_admin(datastores).await;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let auth = &stack.auth_url;

    // 1. An owner (human) and their agent, signed in, contacts through the enrollment, in
    // a direct conversation.
    let signed_in = post_ok(
        &http,
        format!("{auth}/v1/auth/entra"),
        None,
        json!({"token": stub.token(&Uuid::new_v4().to_string()), "handle": "template_owner"}),
    )
    .await;
    let owner = post_ok(
        &http,
        format!("{auth}/v1/devices"),
        Some(signed_in["registration_token"].as_str().unwrap()),
        json!({"registration_id": Uuid::now_v7(), "name": "Owner", "platform": "web"}),
    )
    .await;
    let owner_token = owner["access_token"].as_str().unwrap();
    let owner_id: Uuid = owner["user_id"].as_str().unwrap().parse().unwrap();
    let reservation = post_ok(
        &http,
        format!("{auth}/v1/agents"),
        Some(owner_token),
        json!({"handle": "template_agent", "display_name": "Template Agent"}),
    )
    .await;
    let mut claims = stub.user_claims(TENANT, &Uuid::new_v4().to_string());
    claims.as_object_mut().unwrap().remove("scp");
    claims["idtyp"] = json!("app");
    let registration = post_ok(
        &http,
        format!("{auth}/v1/auth/entra-app"),
        None,
        json!({"token": sign(KEY_A, &claims), "enrollment_code": reservation["enrollment_code"]}),
    )
    .await;
    let agent = post_ok(
        &http,
        format!("{auth}/v1/devices"),
        Some(registration["registration_token"].as_str().unwrap()),
        json!({"registration_id": Uuid::now_v7(), "name": "Agent", "platform": "other"}),
    )
    .await;
    let agent_token = agent["access_token"].as_str().unwrap();
    let agent_id: Uuid = agent["user_id"].as_str().unwrap().parse().unwrap();
    let mut owner_socket = ready(&stack.socket_url, owner_token).await;
    let mut agent_socket = ready(&stack.socket_url, agent_token).await;
    let message_db = sqlx::PgPool::connect(&message_url).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let contacts: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM messaging.contact WHERE \
                 (owner_id = $1 AND peer_id = $2 OR owner_id = $2 AND peer_id = $1) AND state = 3",
            )
            .bind(owner_id)
            .bind(agent_id)
            .fetch_one(&message_db)
            .await
            .unwrap();
            if contacts == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the enrollment makes owner and agent contacts within 30 seconds");
    message_db.close().await;
    let response::Body::DirectOpened(chat) = request(
        &mut owner_socket,
        1,
        request::Body::OpenDirect(OpenDirect {
            peer_user_id: id_bytes(agent_id),
        }),
    )
    .await
    else {
        panic!("direct conversation");
    };
    let chat = chat.conversation_id;

    // 2. The agent's approval card, answered once by the owner; a second answer is refused.
    let card = accepted(&mut agent_socket, &chat, ContentType::Rich, APPROVAL).await;
    let approve = answer(&card, "approve");
    accepted(&mut owner_socket, &chat, ContentType::Answer, &approve).await;
    refused(
        &mut owner_socket,
        &chat,
        ContentType::Answer,
        &approve,
        ErrorCode::Rejected,
    )
    .await;

    // 3. A human may not send an agent-only template.
    refused(
        &mut owner_socket,
        &chat,
        ContentType::Rich,
        APPROVAL,
        ErrorCode::Invalid,
    )
    .await;

    // 4. A submitted template is pending, and a pending template cannot be sent.
    let (status, submitted) = submit(&http, &stack, owner_token, &notice(1)).await;
    assert_eq!(status, StatusCode::CREATED, "{submitted}");
    assert_eq!(submitted["template"], "acme.notice@1");
    assert_eq!(submitted["status"], "pending");
    assert_eq!(submitted["title"], "Notice");
    assert!(submitted["submitted_at"].is_string(), "{submitted}");
    refused(
        &mut owner_socket,
        &chat,
        ContentType::Rich,
        NOTICE,
        ErrorCode::Invalid,
    )
    .await;

    // 5. The admin approves it.
    review(&http, &stack, "acme.notice@1", "approve").await;

    // 6. The catalog serves the built-ins and the approved template, and its ETag.
    let (listed, etag) = catalog(&http, &stack, owner_token, 0).await;
    for template in BUILT_INS {
        assert_eq!(entry(&listed, template)["status"], "approved", "{template}");
    }
    let approved = entry(&listed, "acme.notice@1");
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["definition"]["title"], "Notice", "{approved}");
    let seq = listed["seq"].as_i64().unwrap();
    assert_eq!(etag, format!("\"tpl-0-{seq}\""));
    let unchanged = http
        .get(format!("{auth}/v1/templates?since=0"))
        .bearer_auth(owner_token)
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);

    // 7. Now the owner can send it.
    accepted(&mut owner_socket, &chat, ContentType::Rich, NOTICE).await;

    // 8. Invalid definitions are refused: a URL in a literal, an unknown block type, too
    // many params.
    let mut url_literal = notice(2);
    url_literal["layout"] = json!([
        {"type": "text", "param": "text"},
        {"type": "text", "text": "see https://x.io"},
    ]);
    let mut unknown_block = notice(3);
    unknown_block["layout"] = json!([{"type": "image", "param": "text"}]);
    let mut too_many_params = notice(4);
    too_many_params["params"] = (0..17)
        .map(|n| (format!("p{n}"), json!({"type": "string", "max_len": 10})))
        .collect::<serde_json::Map<_, _>>()
        .into();
    too_many_params["layout"] = json!([{"type": "text", "param": "p0"}]);
    for (name, definition, reason) in [
        ("URL in a literal", url_literal, None),
        ("unknown block type", unknown_block, None),
        (
            "17 params",
            too_many_params,
            Some("needs 0 to 16 params, not 17"),
        ),
    ] {
        let (status, body) = submit(&http, &stack, owner_token, &definition).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {body}");
        assert_eq!(body["error"]["code"], "invalid", "{name}: {body}");
        if let Some(reason) = reason {
            let message = body["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains(reason), "{name}: {body}");
        }
    }

    // 9. An approved version cannot be changed: the catalog still serves the approved one.
    let mut changed = notice(1);
    changed["title"] = json!("Changed notice");
    let (status, body) = submit(&http, &stack, owner_token, &changed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (listed, _) = catalog(&http, &stack, owner_token, 0).await;
    let kept = entry(&listed, "acme.notice@1");
    assert_eq!(kept["definition"]["title"], "Notice", "{kept}");

    // 10. A revoked template leaves the catalog as revoked and can no longer be sent.
    review(&http, &stack, "acme.notice@1", "revoke").await;
    let (changes, _) = catalog(&http, &stack, owner_token, seq).await;
    assert_eq!(
        entry(&changes, "acme.notice@1")["status"],
        "revoked",
        "{changes}"
    );
    refused(
        &mut owner_socket,
        &chat,
        ContentType::Rich,
        NOTICE,
        ErrorCode::Invalid,
    )
    .await;

    let _ = owner_socket.close(None).await;
    let _ = agent_socket.close(None).await;
    stack.stop().await;
}
