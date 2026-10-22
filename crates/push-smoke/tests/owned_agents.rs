//! Real auth/worker/message-svc/gateway with PostgreSQL and Redis. No cloud or mocks.

mod common;

use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use push_auth_core::entra::testing::{KEY_A, StubOpenId, TENANT, sign};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, ContentType, Identify, OpenDirect, Send, request, response};
use push_proto::{decode_frame, encode_frame, id_bytes};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {body}");
    body
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
            Body::Response(answer) if answer.request_id == id => return answer.body.unwrap(),
            Body::Error(error) => panic!("request failed {error:?}"),
            _ => {}
        }
    }
}

async fn converse(sender: &mut Socket, recipient: &mut Socket, conversation: &[u8], author: Uuid) {
    let client_msg_id = id_bytes(Uuid::now_v7());
    send(
        sender,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.to_vec(),
            content_type: ContentType::Text.into(),
            body: b"owned agent conversation".to_vec(),
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
                assert_eq!(entry.sender_id, id_bytes(author));
                assert_eq!(
                    entry.body.as_deref(),
                    Some(b"owned agent conversation".as_slice())
                );
                break;
            }
        }
    }
}

#[tokio::test]
async fn owner_and_agent_chat_as_distinct_users_and_deletion_kicks_the_live_agent() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .with_writer(|| Capture)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    )
    .unwrap();
    let stub = StubOpenId::start().await;
    let (mut datastores, _containers) = common::password_datastores().await;
    let auth_url = datastores
        .auth
        .iter()
        .find(|(key, _)| key == "PUSH_DATABASE_URL")
        .unwrap()
        .1
        .clone();
    let message_url = datastores
        .message_svc
        .iter()
        .find(|(key, _)| key == "PUSH_DATABASE_URL")
        .unwrap()
        .1
        .clone();
    let worker_url = datastores
        .worker
        .iter()
        .find(|(key, _)| key == "PUSH_DATABASE_URL")
        .unwrap()
        .1
        .clone();
    datastores.auth.extend(stub.variables());
    datastores
        .auth
        .push(("PUSH_AUTH_PROVIDERS".into(), "entra,entra-app".into()));
    datastores.worker.extend([
        ("PUSH_EMAIL_SENDER".into(), "none".into()),
        ("PUSH_DEV_MAILBOX".into(), "0".into()),
    ]);
    let stack = common::Stack::start(datastores).await;
    let http = reqwest::Client::new();
    let signed_in = post(
        &http,
        &stack.auth_url,
        "/v1/auth/entra",
        None,
        json!({"token":stub.token(&Uuid::new_v4().to_string()), "handle":"agent_owner"}),
    )
    .await;
    let owner = post(
        &http,
        &stack.auth_url,
        "/v1/devices",
        Some(signed_in["registration_token"].as_str().unwrap()),
        json!({"registration_id":Uuid::now_v7(), "name":"Owner", "platform":"web"}),
    )
    .await;
    let owner_token = owner["access_token"].as_str().unwrap();
    let owner_id: Uuid = owner["user_id"].as_str().unwrap().parse().unwrap();
    let reservation = post(
        &http,
        &stack.auth_url,
        "/v1/agents",
        Some(owner_token),
        json!({"handle":"owned_agent", "display_name":"Owned Agent"}),
    )
    .await;
    let mut claims = stub.user_claims(TENANT, &Uuid::new_v4().to_string());
    claims.as_object_mut().unwrap().remove("scp");
    claims["idtyp"] = json!("app");
    let app_token = sign(KEY_A, &claims);
    let registration = post(
        &http,
        &stack.auth_url,
        "/v1/auth/entra-app",
        None,
        json!({"token":app_token, "enrollment_code":reservation["enrollment_code"]}),
    )
    .await;
    let agent = post(
        &http,
        &stack.auth_url,
        "/v1/devices",
        Some(registration["registration_token"].as_str().unwrap()),
        json!({"registration_id":Uuid::now_v7(), "name":"Agent", "platform":"other"}),
    )
    .await;
    let agent_token = agent["access_token"].as_str().unwrap();
    let agent_id: Uuid = agent["user_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(agent_id, owner_id);
    let mut owner_socket = ready(&stack.socket_url, owner_token).await;
    let mut agent_socket = ready(&stack.socket_url, agent_token).await;

    // The event itself projects both users even if delivered before either device or
    // user.verified. Replaying it has no contact side effects.
    let auth_db = sqlx::PgPool::connect(&auth_url).await.unwrap();
    let message_db = sqlx::PgPool::connect(&message_url).await.unwrap();
    let worker_db = sqlx::PgPool::connect(&worker_url).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let accepted: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM messaging.contact WHERE \
                 (owner_id = $1 AND peer_id = $2 OR owner_id = $2 AND peer_id = $1) AND state = 3",
            )
            .bind(owner_id)
            .bind(agent_id)
            .fetch_one(&message_db)
            .await
            .unwrap();
            if accepted == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("accepted reciprocal contacts");
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
    converse(
        &mut agent_socket,
        &mut owner_socket,
        &chat.conversation_id,
        agent_id,
    )
    .await;
    converse(
        &mut owner_socket,
        &mut agent_socket,
        &chat.conversation_id,
        owner_id,
    )
    .await;

    let profiles: Value = http
        .get(format!(
            "{}/v1/users?ids={agent_id},{owner_id}",
            stack.auth_url
        ))
        .bearer_auth(owner_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(profiles["users"].as_array().unwrap().len(), 2);
    let profile = profiles["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["user_id"] == agent_id.to_string())
        .unwrap();
    assert_eq!(profile["kind"], "agent");
    assert_eq!(profile["owner_user_id"], owner_id.to_string());
    let own: Value = http
        .get(format!("{}/v1/users/me", stack.auth_url))
        .bearer_auth(owner_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(own["kind"], "user");
    assert_eq!(own["owner_user_id"], Value::Null);

    // Prove receipt idempotency survives a later user action, not just immediate replay.
    request(
        &mut owner_socket,
        2,
        request::Body::ContactBlock(v1::ContactBlock {
            user_id: id_bytes(agent_id),
        }),
    )
    .await;
    let replay_id = Uuid::now_v7();
    sqlx::query("INSERT INTO auth.outbox (event_id,kind,payload) VALUES ($1,'agent.enrolled',$2)")
        .bind(replay_id)
        .bind(json!({"user_id":agent_id,"owner_user_id":owner_id}))
        .execute(&auth_db)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let pending: i64 =
                sqlx::query_scalar("SELECT count(*) FROM auth.outbox WHERE kind='agent.enrolled'")
                    .fetch_one(&auth_db)
                    .await
                    .unwrap();
            let queued: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM worker.job WHERE job_id=$1)")
                    .bind(replay_id)
                    .fetch_one(&worker_db)
                    .await
                    .unwrap();
            if pending == 0 && !queued {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    // Wait for the worker's leased delivery too, using its durable receipt count.
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messaging.agent_owner WHERE agent_user_id=$1 AND owner_user_id=$2",
    )
    .bind(agent_id)
    .bind(owner_id)
    .fetch_one(&message_db)
    .await
    .unwrap();
    assert_eq!(receipts, 1);
    let blocked: i16 =
        sqlx::query_scalar("SELECT state FROM messaging.contact WHERE owner_id=$1 AND peer_id=$2")
            .bind(owner_id)
            .bind(agent_id)
            .fetch_one(&message_db)
            .await
            .unwrap();
    assert_eq!(blocked, 4, "replayed consent never overrides a later block");

    let deleted = http
        .delete(format!("{}/v1/agents/{agent_id}", stack.auth_url))
        .bearer_auth(owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    let close = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Message::Close(Some(frame)) = agent_socket.next().await.unwrap().unwrap() {
                break u16::from(frame.code);
            }
        }
    })
    .await
    .expect("device.revoked closes live socket");
    assert_eq!(close, 4003);
    for path in ["/v1/users/me", "/v1/devices"] {
        assert_eq!(
            http.get(format!("{}{path}", stack.auth_url))
                .bearer_auth(agent_token)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        http.post(format!("{}/v1/auth/refresh", stack.auth_url))
            .json(&json!({"refresh_token":agent["refresh_token"]}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        http.post(format!("{}/v1/devices", stack.auth_url))
            .bearer_auth(registration["registration_token"].as_str().unwrap())
            .json(&json!({"registration_id":Uuid::now_v7(),"name":"Revival","platform":"other"}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        http.post(format!("{}/v1/auth/entra-app", stack.auth_url))
            .json(&json!({"token":app_token}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let (mut revived, _) = connect_async(&stack.socket_url).await.unwrap();
    send(
        &mut revived,
        Body::Identify(Identify {
            token: agent_token.to_owned(),
        }),
    )
    .await;
    let Message::Close(Some(frame)) = revived.next().await.unwrap().unwrap() else {
        panic!("revoked identify closes");
    };
    assert_eq!(u16::from(frame.code), 4003);
    let messages: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messaging.message WHERE sender_id=$1")
            .bind(agent_id)
            .fetch_one(&message_db)
            .await
            .unwrap();
    assert!(messages > 0, "deletion preserves authored history");
    owner_socket.close(None).await.unwrap();
    auth_db.close().await;
    message_db.close().await;
    worker_db.close().await;
    stack.stop().await;
    let log = String::from_utf8(LOG.lock().unwrap().clone()).unwrap();
    assert!(!log.is_empty());
    for secret in [
        app_token.as_str(),
        reservation["enrollment_code"].as_str().unwrap(),
        registration["registration_token"].as_str().unwrap(),
        agent_token,
        agent["refresh_token"].as_str().unwrap(),
    ] {
        assert!(
            !log.contains(secret),
            "an enrollment/session secret reached the audit log"
        );
    }
}
