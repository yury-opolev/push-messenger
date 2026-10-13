//! Email on a real PostgreSQL (Docker must be running): sending once, suppression,
//! retries and refusals, delivery webhooks, and the HTTP API.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use push_email::{EmailSender, LogSender, OutgoingEmail, ResendWebhooks, SendError, Templates};
use push_worker::deliveries::{Deliveries, Received};
use push_worker::email::{EMAIL_MAX_ATTEMPTS, Mailbox, MailboxSender, Mailer};
use push_worker::http::{Http, WEBHOOK_PATH, router};
use push_worker::migrate_database;
use push_worker::queue::{Job, Outcome};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tower::ServiceExt;
use uuid::Uuid;

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
const LINK: &str = "https://push.test/v1/auth/verify?token=secret-token";

async fn database() -> (ContainerAsync<Postgres>, PgPool) {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    migrate_database(&url).await.unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    (container, pool)
}

/// A sender that records what it was given and answers from a script (then "re-N").
#[derive(Default)]
struct Scripted {
    sent: Mutex<Vec<(OutgoingEmail, Uuid)>>,
    answers: Mutex<VecDeque<Result<String, SendError>>>,
}

#[async_trait]
impl EmailSender for Scripted {
    async fn send(&self, email: &OutgoingEmail, key: Uuid) -> Result<String, SendError> {
        let mut sent = self.sent.lock().unwrap();
        sent.push((email.clone(), key));
        let answer = self.answers.lock().unwrap().pop_front();
        answer.unwrap_or_else(|| Ok(format!("re-{}", sent.len())))
    }
}

fn verify_email(to: &str) -> Job {
    Job {
        id: Uuid::now_v7(),
        kind: "email.send".to_owned(),
        payload: json!({
            "user_id": Uuid::now_v7(),
            "to": to,
            "template": "verify_email",
            "params": {"link": LINK, "handle": "alice", "expires_in_minutes": 30},
        }),
        attempts: 1,
        claimed_at: Utc::now(),
    }
}

async fn message(pool: &PgPool, id: Uuid) -> Option<(i16, Option<String>, Value, String)> {
    sqlx::query_as(
        "SELECT status, provider_message_id, params, recipient_domain \
         FROM worker.email_message WHERE email_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn an_email_is_rendered_sent_once_and_recorded_without_its_link() {
    let (_container, pool) = database().await;
    let sender = Arc::new(Scripted::default());
    let mailer = Mailer::new(pool.clone(), sender.clone(), Templates::default());
    let job = verify_email("alice@Example.com");
    assert_eq!(mailer.send(&job).await, Outcome::Done);
    assert_eq!(mailer.send(&job).await, Outcome::Done, "a second attempt");

    let sent = sender.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "sent once");
    let (email, key) = &sent[0];
    assert_eq!(*key, job.id);
    assert_eq!(email.to, "alice@Example.com");
    assert!(email.content.text.contains(LINK));
    assert_eq!(
        message(&pool, job.id).await,
        Some((
            2,
            Some("re-1".to_owned()),
            json!({"handle": "alice", "expires_in_minutes": 30}),
            "example.com".to_owned()
        ))
    );
}

#[tokio::test]
async fn a_suppressed_address_gets_nothing() {
    let (_container, pool) = database().await;
    sqlx::query("INSERT INTO worker.email_suppression (address, reason) VALUES ($1, 1)")
        .bind("ALICE@example.com")
        .execute(&pool)
        .await
        .unwrap();
    let sender = Arc::new(Scripted::default());
    let mailer = Mailer::new(pool.clone(), sender.clone(), Templates::default());
    let job = verify_email("alice@example.com");
    assert_eq!(mailer.send(&job).await, Outcome::Done);
    assert!(sender.sent.lock().unwrap().is_empty());
    assert_eq!(message(&pool, job.id).await.unwrap().0, 3);
}

#[tokio::test]
async fn failures_are_retried_until_the_link_expires_and_refusals_fail_at_once() {
    let (_container, pool) = database().await;
    let sender = Arc::new(Scripted::default());
    let mailer = Mailer::new(pool.clone(), sender.clone(), Templates::default());
    let outage = || Err(SendError::Retryable("Resend answered 503".to_owned()));

    let mut job = verify_email("alice@example.com");
    sender.answers.lock().unwrap().push_back(outage());
    assert_eq!(
        mailer.send(&job).await,
        Outcome::Failed {
            reason: "Resend answered 503".to_owned(),
            max_attempts: None,
        }
    );
    assert_eq!(message(&pool, job.id).await.unwrap().0, 1, "still queued");
    job.attempts = EMAIL_MAX_ATTEMPTS;
    sender.answers.lock().unwrap().push_back(outage());
    assert_eq!(
        mailer.send(&job).await,
        Outcome::Poison("Resend answered 503".to_owned())
    );
    assert_eq!(message(&pool, job.id).await.unwrap().0, 4, "failed");

    let refused = verify_email("bob@example.com");
    sender
        .answers
        .lock()
        .unwrap()
        .push_back(Err(SendError::Permanent("Resend answered 422".to_owned())));
    assert!(matches!(mailer.send(&refused).await, Outcome::Poison(_)));
    assert_eq!(message(&pool, refused.id).await.unwrap().0, 4);

    let mut unknown = verify_email("carol@example.com");
    unknown.payload["template"] = json!("welcome");
    assert!(matches!(mailer.send(&unknown).await, Outcome::Poison(_)));
    assert_eq!(message(&pool, unknown.id).await.unwrap().0, 4);
    let mut malformed = verify_email("dave@example.com");
    malformed.payload = json!({"template": "verify_email"});
    assert!(matches!(mailer.send(&malformed).await, Outcome::Poison(_)));
    assert_eq!(sender.sent.lock().unwrap().len(), 3);
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A Resend webhook of `kind` about the email `email_id` to `to`, signed now.
fn webhook(
    id: &str,
    kind: &str,
    email_id: &str,
    to: &str,
    bounce: &str,
) -> (Vec<(String, String)>, Vec<u8>) {
    let body = json!({
        "type": kind,
        "created_at": "2026-10-13T10:00:00.000Z",
        "data": {"email_id": email_id, "to": [to], "bounce": {"type": bounce}},
    })
    .to_string()
    .into_bytes();
    signed(id, body)
}

/// `body` with Resend's signature headers, signed now.
fn signed(id: &str, body: Vec<u8>) -> (Vec<(String, String)>, Vec<u8>) {
    let timestamp = now();
    let signature = ResendWebhooks::new(SECRET)
        .unwrap()
        .sign(id, timestamp, &body);
    let headers = vec![
        ("svix-id".to_owned(), id.to_owned()),
        ("svix-timestamp".to_owned(), timestamp.to_string()),
        ("svix-signature".to_owned(), signature),
    ];
    (headers, body)
}

async fn receive(
    deliveries: &Deliveries,
    (headers, body): &(Vec<(String, String)>, Vec<u8>),
) -> Result<Received, push_worker::deliveries::DeliveryError> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    deliveries.receive(&header, body).await
}

async fn suppressed(pool: &PgPool) -> Vec<(String, i16)> {
    sqlx::query_as("SELECT address::text, reason FROM worker.email_suppression ORDER BY address")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn webhooks_are_recorded_once_and_hard_bounces_and_complaints_suppress() {
    let (_container, pool) = database().await;
    let deliveries = Deliveries::new(pool.clone(), Arc::new(ResendWebhooks::new(SECRET).unwrap()));
    // An email the worker sent to Alice, whose bounce names another address.
    let mailer = Mailer::new(
        pool.clone(),
        Arc::new(Scripted::default()),
        Templates::default(),
    );
    mailer.send(&verify_email("alice@example.com")).await;

    let hard = webhook(
        "msg_1",
        "email.bounced",
        "re-1",
        "alias@example.com",
        "Permanent",
    );
    assert_eq!(
        receive(&deliveries, &hard).await.unwrap(),
        Received::Recorded { suppressed: 2 }
    );
    assert_eq!(receive(&deliveries, &hard).await.unwrap(), Received::Replay);
    let soft = webhook(
        "msg_2",
        "email.bounced",
        "re-9",
        "bob@example.com",
        "Transient",
    );
    assert_eq!(
        receive(&deliveries, &soft).await.unwrap(),
        Received::Recorded { suppressed: 0 }
    );
    let complaint = webhook("msg_3", "email.complained", "re-8", "carol@example.com", "");
    assert_eq!(
        receive(&deliveries, &complaint).await.unwrap(),
        Received::Recorded { suppressed: 1 }
    );
    let opened = webhook("msg_4", "email.opened", "re-7", "dave@example.com", "");
    assert_eq!(
        receive(&deliveries, &opened).await.unwrap(),
        Received::Ignored
    );
    // Signed, but not an event we can read: dropped, yet acknowledged.
    for body in [
        b"not json".to_vec(),
        br#"{"type":"email.bounced"}"#.to_vec(),
    ] {
        assert_eq!(
            receive(&deliveries, &signed("msg_9", body)).await.unwrap(),
            Received::Malformed
        );
    }

    assert_eq!(
        suppressed(&pool).await,
        [
            ("alias@example.com".to_owned(), 1),
            ("alice@example.com".to_owned(), 1),
            ("carol@example.com".to_owned(), 2),
        ]
    );
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM worker.email_event")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 3);

    let (mut forged, body) = webhook("msg_5", "email.complained", "re-6", "eve@example.com", "");
    forged[2].1 = "v1,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned();
    assert!(receive(&deliveries, &(forged, body)).await.is_err());
    assert_eq!(suppressed(&pool).await.len(), 3);
}

async fn call(app: &axum::Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

#[tokio::test]
async fn the_http_api_takes_signed_webhooks_and_serves_the_mailbox_only_in_dev() {
    let (_container, pool) = database().await;
    let deliveries = Deliveries::new(pool.clone(), Arc::new(ResendWebhooks::new(SECRET).unwrap()));
    let mailbox = Mailbox::default();
    let mailer = Mailer::new(
        pool,
        Arc::new(MailboxSender::new(LogSender, mailbox.clone())),
        Templates::default(),
    );
    mailer.send(&verify_email("alice@example.com")).await;
    let dev = router(Http {
        deliveries: Some(deliveries.clone()),
        mailbox: Some(mailbox),
    });
    let production = router(Http {
        deliveries: Some(deliveries),
        mailbox: None,
    });

    let (headers, body) = webhook("msg_1", "email.delivered", "log-1", "alice@example.com", "");
    let post = |headers: &[(String, String)], body: Vec<u8>| {
        let mut request = Request::post(WEBHOOK_PATH);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request.body(Body::from(body)).unwrap()
    };
    assert_eq!(
        call(&production, post(&headers, body.clone())).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&production, post(&headers[..2], body.clone())).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (headers, _) = webhook("msg_2", "email.delivered", "log-1", "alice@example.com", "");
    assert_eq!(
        call(&production, post(&headers, b"not json".to_vec()))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Signed but malformed: 204, or Resend would send it again for days.
    let (headers, body) = signed("msg_3", b"not json".to_vec());
    assert_eq!(
        call(&production, post(&headers, body)).await.0,
        StatusCode::NO_CONTENT
    );

    let mailbox_request = || {
        Request::get("/dev/mailbox?to=alice@example.com")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        call(&production, mailbox_request()).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, body) = call(&dev, mailbox_request()).await;
    assert_eq!(status, StatusCode::OK);
    let emails: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(emails["emails"][0]["template"], "verify_email");
    assert!(emails["emails"][0]["text"].as_str().unwrap().contains(LINK));
    let disabled = router(Http::default());
    assert_eq!(
        call(&disabled, post(&[], Vec::new())).await.0,
        StatusCode::NOT_FOUND
    );
}
