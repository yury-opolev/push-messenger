//! The whole stack with Microsoft Entra ID sign-in only (spec 2026-10-19 AC2): auth-svc
//! trusts the stub issuer and serves no email route, the worker sends no email, and two
//! users sign in with Entra ID tokens through push-sdk, become contacts and chat; one picks
//! another handle and the other finds her by it; signing in again keeps the device. No
//! token, object id, name or address reaches a log.
//!
//! One test per binary: it owns the global log capture.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::Stack;
use push_auth::entra::testing::StubOpenId;
use push_sdk::{
    Client, Config, ConnectionState, ContactState, Event, Events, MemorySecretStore, MessageStatus,
    Platform, UserId,
};

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

struct App {
    client: Client,
    events: Events,
    user: UserId,
}

impl App {
    /// Signs `name` in with an Entra ID token for `object_id`, registering a device, and
    /// connects (the gateway answers 4006 until the worker relays the device; the SDK
    /// reconnects by itself).
    async fn sign_in(
        stack: &Stack,
        stub: &StubOpenId,
        data_dir: &Path,
        name: &str,
        object_id: &str,
        tokens: &mut Vec<String>,
    ) -> Self {
        let database = data_dir.join(format!("{name}.db"));
        let mut config = Config::new(
            &stack.auth_url,
            &stack.socket_url,
            &database.to_string_lossy(),
        );
        // Plain HTTP to this machine's stack, as the local smoke test does.
        config.allow_insecure_transport = true;
        let (client, events) = Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
        let token = stub.token(object_id);
        tokens.push(token.clone());
        let account = client
            .sign_in_with_entra(
                &token,
                Some(name),
                &format!("{name}'s laptop"),
                Platform::Desktop,
            )
            .await
            .expect("an Entra ID sign-in");
        client.connect().unwrap();
        let mut app = Self {
            client,
            events,
            user: account.user_id,
        };
        app.wait("Connected", |event| {
            matches!(
                event,
                Event::ConnectionStateChanged(ConnectionState::Connected)
            )
        })
        .await;
        app
    }

    async fn wait(&mut self, what: &str, wanted: impl Fn(&Event) -> bool) -> Event {
        let events = &mut self.events;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let event = events.next().await.expect("the client is open");
                if wanted(&event) {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {what} within 60 s"))
    }
}

#[tokio::test]
async fn entra_users_sign_in_and_chat_with_no_email_anywhere() {
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(|| Capture)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let stub = StubOpenId::start().await;
    let (mut datastores, _containers) = common::password_datastores().await;
    datastores.auth.extend(stub.variables());
    datastores.worker.extend([
        ("PUSH_EMAIL_SENDER".to_owned(), "none".to_owned()),
        ("PUSH_DEV_MAILBOX".to_owned(), "0".to_owned()),
    ]);
    let worker_database = datastores
        .worker
        .iter()
        .find(|(name, _)| name == "PUSH_DATABASE_URL")
        .map(|(_, value)| value.clone())
        .unwrap();
    let stack = Stack::start(datastores).await;

    let http = reqwest::Client::new();
    for path in [
        "/v1/auth/signup",
        "/v1/auth/login",
        "/v1/auth/password/forgot",
        "/v1/auth/verify/resend",
    ] {
        let status = http
            .post(format!("{}{path}", stack.auth_url))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 404, "{path}");
    }

    let data_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-entra-sign-in-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let (erin_oid, finn_oid) = (
        uuid::Uuid::now_v7().to_string(),
        uuid::Uuid::now_v7().to_string(),
    );
    let mut tokens = Vec::new();
    let mut erin = App::sign_in(&stack, &stub, &data_dir, "erin", &erin_oid, &mut tokens).await;
    let mut finn = App::sign_in(&stack, &stub, &data_dir, "finn", &finn_oid, &mut tokens).await;

    let invite = erin.client.create_invite(600, 1).await.unwrap();
    assert_eq!(
        finn.client.redeem_invite(&invite.code).await.unwrap(),
        erin.user
    );
    let finn_user = finn.user;
    erin.wait("the new contact", |event| {
        matches!(event, Event::ContactChanged(contact)
            if contact.user_id == finn_user && contact.state == ContactState::Accepted)
    })
    .await;
    let chat = erin.client.open_direct(finn.user).await.unwrap().id;
    let text = format!("hello over Entra ID {}", uuid::Uuid::now_v7());
    let id = erin.client.send(chat, &text).unwrap().client_msg_id;
    let Event::MessageAdded(received) = finn
        .wait(
            "the message",
            |event| matches!(event, Event::MessageAdded(message) if message.client_msg_id == id),
        )
        .await
    else {
        unreachable!("the wait accepts only MessageAdded");
    };
    assert_eq!(received.body.as_deref(), Some(text.as_str()));
    assert_eq!(received.status, MessageStatus::Received);

    // Erin picks a handle of her own; Finn finds her by it.
    let renamed = erin
        .client
        .update_profile(None, Some("erin_entra"))
        .await
        .unwrap();
    assert_eq!(renamed.handle, "erin_entra");
    assert_eq!(
        finn.client.lookup("erin_entra").await.unwrap().user_id,
        erin.user
    );

    // Signing in again keeps the device.
    let device = erin.client.account().unwrap().unwrap().device_id;
    let again = stub.token(&erin_oid);
    tokens.push(again.clone());
    let account = erin
        .client
        .sign_in_with_entra(&again, None, "erin's laptop", Platform::Desktop)
        .await
        .unwrap();
    assert_eq!(account.device_id, device);

    // No email was ever queued, sent or buried.
    let worker = sqlx::PgPool::connect(&worker_database).await.unwrap();
    let emails: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM worker.job WHERE kind = 'email.send') \
         + (SELECT count(*) FROM worker.email_message)",
    )
    .fetch_one(&worker)
    .await
    .unwrap();
    assert_eq!(emails, 0);
    worker.close().await;

    for app in [erin, finn] {
        app.client.close().await.unwrap();
    }
    stack.stop().await;
    let log = String::from_utf8(LOG.lock().unwrap().clone()).unwrap();
    assert!(!log.is_empty(), "the log was captured");
    for secret in tokens.iter().map(String::as_str).chain([
        erin_oid.as_str(),
        finn_oid.as_str(),
        "Test User",
        "test.user@contoso.test",
    ]) {
        let leak = log
            .lines()
            .find(|line| line.contains(secret))
            .map(|line| line.replace(secret, "<SECRET>"));
        assert!(leak.is_none(), "a secret reached a log: {leak:?}");
    }
    let _ = std::fs::remove_dir_all(&data_dir);
}
