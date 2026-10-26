//! The template routes (spec 2026-10-26 rich message templates, "HTTP (auth-svc)"): the
//! public submission and catalog, and the admin review, with a fake registry in place of
//! message-svc.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use super::{Offer, World, capture_logs, password_user, security_events, service, world};
use crate::admin::{AdminApi, admin_router};
use crate::api::{Api, router};
use crate::directory::NoDirectory;
use crate::error::AuthError;
use crate::limits::{Limits, MemoryCounter};
use crate::service::SignIn;
use crate::templates::{
    Catalog, CatalogEntry, NoTemplates, ReviewAction, TemplateDetail, TemplateEvent,
    TemplateRegistry, TemplateStatus, TemplateSummary,
};

const ADMIN_TOKEN: &str = "admin-token-of-the-auth-svc-tests-0123456789";

/// A review as the registry received it: ref, action, reviewer, note.
type Reviewed = (String, ReviewAction, String, Option<String>);

/// message-svc's template registry in miniature: it records what it was asked and answers
/// fixed values, or `refuse` when set.
#[derive(Debug, Default)]
struct FakeRegistry {
    submitted: Mutex<Vec<(Uuid, Vec<u8>)>>,
    catalogs: Mutex<Vec<(i64, u32)>>,
    listed: Mutex<Vec<Option<TemplateStatus>>>,
    reviews: Mutex<Vec<Reviewed>>,
    /// Every call fails with this error when set.
    refuse: Mutex<Option<AuthError>>,
}

impl FakeRegistry {
    fn refusing(&self, error: AuthError) {
        *self.refuse.lock().unwrap() = Some(error);
    }

    fn refusal(&self) -> Result<(), AuthError> {
        match self.refuse.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn at(micros: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(micros).unwrap()
}

fn summary(template: &str, status: TemplateStatus) -> TemplateSummary {
    TemplateSummary {
        template: template.to_owned(),
        status,
        title: "Order shipped".to_owned(),
        submitted_by: Some(Uuid::from_u128(7)),
        submitted_at: at(1_790_000_000_000_000),
        reviewed_at: None,
        seq: None,
    }
}

#[async_trait::async_trait]
impl TemplateRegistry for FakeRegistry {
    async fn submit(&self, user: Uuid, definition: Vec<u8>) -> Result<TemplateSummary, AuthError> {
        self.refusal()?;
        self.submitted.lock().unwrap().push((user, definition));
        Ok(summary("acme.shipped@1", TemplateStatus::Pending))
    }

    async fn catalog(&self, since: i64, limit: u32) -> Result<Catalog, AuthError> {
        self.refusal()?;
        self.catalogs.lock().unwrap().push((since, limit));
        Ok(Catalog {
            seq: 9,
            more: false,
            entries: vec![
                CatalogEntry {
                    template: "agent.reply@1".to_owned(),
                    status: TemplateStatus::Approved,
                    seq: 8,
                    definition: Some(json!({"format": "tpl/1", "title": "Reply"})),
                },
                CatalogEntry {
                    template: "acme.old@1".to_owned(),
                    status: TemplateStatus::Revoked,
                    seq: 9,
                    definition: None,
                },
            ],
        })
    }

    async fn list(
        &self,
        status: Option<TemplateStatus>,
        _limit: u32,
    ) -> Result<Vec<TemplateSummary>, AuthError> {
        self.refusal()?;
        self.listed.lock().unwrap().push(status);
        Ok(vec![summary("acme.shipped@1", TemplateStatus::Pending)])
    }

    async fn get(&self, template: &str) -> Result<TemplateDetail, AuthError> {
        self.refusal()?;
        if template != "acme.shipped@1" {
            return Err(AuthError::TemplateNotFound);
        }
        Ok(TemplateDetail {
            summary: summary(template, TemplateStatus::Pending),
            definition: json!({"format": "tpl/1", "title": "Order shipped"}),
            outline: "[Order shipped]".to_owned(),
            events: vec![TemplateEvent {
                action: "submit".to_owned(),
                actor: Some(Uuid::from_u128(7)),
                reviewer: None,
                note: None,
                at: at(1_790_000_000_000_000),
            }],
        })
    }

    async fn review(
        &self,
        template: &str,
        action: ReviewAction,
        reviewer: &str,
        note: Option<&str>,
    ) -> Result<TemplateSummary, AuthError> {
        self.refusal()?;
        self.reviews.lock().unwrap().push((
            template.to_owned(),
            action,
            reviewer.to_owned(),
            note.map(str::to_owned),
        ));
        let mut reviewed = summary(template, TemplateStatus::Approved);
        reviewed.reviewed_at = Some(at(1_790_000_100_000_000));
        reviewed.seq = Some(12);
        Ok(reviewed)
    }
}

fn public(world: &World, templates: Arc<dyn TemplateRegistry>) -> Router {
    router(Api {
        auth: world.auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::new(NoDirectory),
        templates,
    })
}

/// The admin router over a service that never reaches its database: the template routes
/// do not need it.
fn admin(templates: Arc<dyn TemplateRegistry>) -> Router {
    let pool = PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none").unwrap();
    admin_router(
        AdminApi::new(
            service(pool, SignIn::default()),
            Limits::new(Arc::new(MemoryCounter::default())),
            ADMIN_TOKEN,
        )
        .with_templates(templates),
    )
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// One request; `content_type` is sent with a body.
async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<(&str, Vec<u8>)>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-real-ip", "198.51.100.7");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = match body {
        Some((content_type, bytes)) => request
            .header("content-type", content_type)
            .body(Body::from(bytes)),
        None => request.body(Body::empty()),
    }
    .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 9], 40000))));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: body.to_vec(),
    }
}

const DEFINITION: &str = r#"{"format":"tpl/1",  "publisher":"acme","name":"shipped","version":1, "title":"Order shipped"}"#;

#[tokio::test]
async fn a_signed_in_user_submits_a_definition_forwarded_byte_for_byte() {
    capture_logs();
    let world = world(Offer::Password).await;
    let user = password_user(&world.auth, &world.pool, "tess", 1)
        .await
        .remove(0);
    let registry = Arc::new(FakeRegistry::default());
    let routes = public(&world, registry.clone());
    let json = Some(("application/json", DEFINITION.as_bytes().to_vec()));

    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        Some(&user.access_token),
        &[],
        json.clone(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{:?}", answer.json());
    assert_eq!(
        answer.json(),
        json!({
            "template": "acme.shipped@1",
            "status": "pending",
            "title": "Order shipped",
            "submitted_at": "2026-09-21T14:13:20.000Z",
        })
    );
    assert_eq!(answer.headers["cache-control"], "no-store");
    assert_eq!(
        *registry.submitted.lock().unwrap(),
        vec![(user.user_id, DEFINITION.as_bytes().to_vec())],
        "the raw body, not a re-serialisation"
    );
    assert!(security_events().iter().any(|event| {
        event["event"] == "auth.template.submitted" && event["user_id"] == user.user_id.to_string()
    }));

    // Without a token, nothing is forwarded.
    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        None,
        &[],
        json.clone(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    // Only JSON, and at most 16 KiB.
    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        Some(&user.access_token),
        &[],
        Some(("text/plain", DEFINITION.as_bytes().to_vec())),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert_eq!(answer.json()["error"]["code"], "invalid");
    let large = format!(r#"{{"title":"{}"}}"#, "x".repeat(16 * 1024));
    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        Some(&user.access_token),
        &[],
        Some(("application/json", large.into_bytes())),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert_eq!(answer.json()["error"]["code"], "invalid");
    assert_eq!(registry.submitted.lock().unwrap().len(), 1);

    // message-svc's refusals keep their message.
    registry.refusing(AuthError::TemplateInvalid(
        "layout: at most 32 blocks".to_owned(),
    ));
    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        Some(&user.access_token),
        &[],
        json.clone(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert_eq!(answer.json()["error"]["code"], "invalid");
    assert_eq!(
        answer.json()["error"]["message"],
        "layout: at most 32 blocks"
    );
    registry.refusing(AuthError::TemplateConflict(
        "the publisher acme belongs to another account".to_owned(),
    ));
    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        Some(&user.access_token),
        &[],
        json.clone(),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert_eq!(answer.json()["error"]["code"], "conflict");
    assert_eq!(
        answer.json()["error"]["message"],
        "the publisher acme belongs to another account"
    );
    assert!(security_events().iter().any(|event| {
        event["event"] == "auth.template.refused" && event["user_id"] == user.user_id.to_string()
    }));

    // Ten submissions per hour per user: three counted so far (a refusal counts too).
    *registry.refuse.lock().unwrap() = None;
    for _ in 0..7 {
        let answer = send(
            &routes,
            Method::POST,
            "/v1/templates",
            Some(&user.access_token),
            &[],
            json.clone(),
        )
        .await;
        assert_eq!(answer.status, StatusCode::CREATED);
    }
    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        Some(&user.access_token),
        &[],
        json,
    )
    .await;
    assert_eq!(answer.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn the_catalog_answers_with_an_etag_and_304_when_unchanged() {
    let world = world(Offer::Password).await;
    let user = password_user(&world.auth, &world.pool, "cara", 1)
        .await
        .remove(0);
    let registry = Arc::new(FakeRegistry::default());
    let routes = public(&world, registry.clone());
    let token = Some(user.access_token.as_str());

    let answer = send(
        &routes,
        Method::GET,
        "/v1/templates?since=3",
        token,
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.json());
    assert_eq!(
        answer.json(),
        json!({
            "seq": 9,
            "more": false,
            "templates": [
                {"template": "agent.reply@1", "status": "approved", "seq": 8,
                 "definition": {"format": "tpl/1", "title": "Reply"}},
                {"template": "acme.old@1", "status": "revoked", "seq": 9},
            ],
        })
    );
    assert_eq!(answer.headers["etag"], "\"tpl-3-9\"");
    assert_eq!(answer.headers["cache-control"], "private, no-cache");
    assert_eq!(*registry.catalogs.lock().unwrap(), vec![(3, 0)]);

    let answer = send(&routes, Method::GET, "/v1/templates", token, &[], None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.headers["etag"], "\"tpl-0-9\"");

    for matching in [
        "\"tpl-3-9\"",
        "\"tpl-1-1\", \"tpl-3-9\"",
        "*",
        "W/\"tpl-3-9\"",
    ] {
        let answer = send(
            &routes,
            Method::GET,
            "/v1/templates?since=3",
            token,
            &[("if-none-match", matching)],
            None,
        )
        .await;
        assert_eq!(answer.status, StatusCode::NOT_MODIFIED, "{matching}");
        assert_eq!(answer.headers["etag"], "\"tpl-3-9\"");
        assert_eq!(answer.headers["cache-control"], "private, no-cache");
        assert!(answer.body.is_empty());
    }
    let answer = send(
        &routes,
        Method::GET,
        "/v1/templates?since=3",
        token,
        &[("if-none-match", "\"tpl-3-8\"")],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);

    for bad in ["since=-1", "since=x", "since=", "since=1.5"] {
        let answer = send(
            &routes,
            Method::GET,
            &format!("/v1/templates?{bad}"),
            token,
            &[],
            None,
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(answer.json()["error"]["code"], "invalid");
        assert_eq!(answer.headers["cache-control"], "no-store");
    }
    let answer = send(&routes, Method::GET, "/v1/templates", None, &[], None).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn without_message_svc_the_template_routes_are_unavailable() {
    let world = world(Offer::Password).await;
    let user = password_user(&world.auth, &world.pool, "nora", 1)
        .await
        .remove(0);
    let routes = public(&world, Arc::new(NoTemplates));
    let token = Some(user.access_token.as_str());
    let answer = send(
        &routes,
        Method::POST,
        "/v1/templates",
        token,
        &[],
        Some(("application/json", DEFINITION.as_bytes().to_vec())),
    )
    .await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    let answer = send(&routes, Method::GET, "/v1/templates", token, &[], None).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.json()["error"]["code"], "unavailable");

    let admin = admin(Arc::new(NoTemplates));
    let answer = send(
        &admin,
        Method::GET,
        "/v1/admin/templates",
        Some(ADMIN_TOKEN),
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn admin_routes_need_the_admin_token() {
    let registry = Arc::new(FakeRegistry::default());
    let admin = admin(registry.clone());
    let review = || Some(("application/json", br#"{"reviewer":"ops"}"#.to_vec()));
    for (method, uri, body) in [
        (Method::GET, "/v1/admin/templates", None),
        (Method::GET, "/v1/admin/templates/acme.shipped@1", None),
        (
            Method::POST,
            "/v1/admin/templates/acme.shipped@1/approve",
            review(),
        ),
        (
            Method::POST,
            "/v1/admin/templates/acme.shipped@1/reject",
            review(),
        ),
        (
            Method::POST,
            "/v1/admin/templates/acme.shipped@1/revoke",
            review(),
        ),
    ] {
        for token in [None, Some("not-the-admin-token-but-long-enough-0123")] {
            let answer = send(&admin, method.clone(), uri, token, &[], body.clone()).await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
            assert_eq!(answer.headers["cache-control"], "no-store");
        }
    }
    assert!(registry.listed.lock().unwrap().is_empty());
    assert!(registry.reviews.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_operator_lists_inspects_and_reviews_templates() {
    capture_logs();
    let registry = Arc::new(FakeRegistry::default());
    let admin = admin(registry.clone());
    let token = Some(ADMIN_TOKEN);
    let expected_summary = json!({
        "template": "acme.shipped@1",
        "status": "pending",
        "title": "Order shipped",
        "submitted_by": Uuid::from_u128(7),
        "submitted_at": "2026-09-21T14:13:20.000Z",
        "reviewed_at": null,
        "seq": null,
    });

    let answer = send(&admin, Method::GET, "/v1/admin/templates", token, &[], None).await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.json());
    assert_eq!(answer.json(), json!({"templates": [expected_summary]}));
    let answer = send(
        &admin,
        Method::GET,
        "/v1/admin/templates?status=approved",
        token,
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        *registry.listed.lock().unwrap(),
        vec![None, Some(TemplateStatus::Approved)]
    );
    let answer = send(
        &admin,
        Method::GET,
        "/v1/admin/templates?status=lost",
        token,
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);

    let answer = send(
        &admin,
        Method::GET,
        "/v1/admin/templates/acme.shipped@1",
        token,
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.json());
    let mut expected = expected_summary.clone();
    let fields = expected.as_object_mut().unwrap();
    fields.insert(
        "definition".to_owned(),
        json!({"format": "tpl/1", "title": "Order shipped"}),
    );
    fields.insert("outline".to_owned(), json!("[Order shipped]"));
    fields.insert(
        "events".to_owned(),
        json!([{"action": "submit", "actor": Uuid::from_u128(7), "reviewer": null,
                "note": null, "at": "2026-09-21T14:13:20.000Z"}]),
    );
    assert_eq!(answer.json(), expected);
    let answer = send(
        &admin,
        Method::GET,
        "/v1/admin/templates/acme.other@1",
        token,
        &[],
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.json()["error"]["code"], "not_found");
    // A ref never names another path, encoded or not.
    for bad in [
        "/v1/admin/templates/acme%2Fshipped@1",
        "/v1/admin/templates/..%2F..%2Fusers",
    ] {
        let answer = send(&admin, Method::GET, bad, token, &[], None).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }

    let answer = send(
        &admin,
        Method::POST,
        "/v1/admin/templates/acme.shipped@1/approve",
        token,
        &[],
        Some((
            "application/json",
            br#"{"reviewer":"ops-alice","note":"looks fine"}"#.to_vec(),
        )),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{:?}", answer.json());
    assert_eq!(answer.json()["status"], "approved");
    assert_eq!(answer.json()["seq"], 12);
    assert_eq!(answer.json()["reviewed_at"], "2026-09-21T14:15:00.000Z");
    for action in ["reject", "revoke"] {
        let answer = send(
            &admin,
            Method::POST,
            &format!("/v1/admin/templates/acme.shipped@1/{action}"),
            token,
            &[],
            Some(("application/json", br#"{"reviewer":"ops-bob"}"#.to_vec())),
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{action}");
    }
    assert_eq!(
        *registry.reviews.lock().unwrap(),
        vec![
            (
                "acme.shipped@1".to_owned(),
                ReviewAction::Approve,
                "ops-alice".to_owned(),
                Some("looks fine".to_owned())
            ),
            (
                "acme.shipped@1".to_owned(),
                ReviewAction::Reject,
                "ops-bob".to_owned(),
                None
            ),
            (
                "acme.shipped@1".to_owned(),
                ReviewAction::Revoke,
                "ops-bob".to_owned(),
                None
            ),
        ]
    );
    assert!(security_events().iter().any(|event| {
        event["event"] == "auth.admin.template_reviewed"
            && event["template"] == "acme.shipped@1"
            && event["action"] == "approve"
            && event["reviewer"] == "ops-alice"
            && event["ip"] == "10.0.0.9"
    }));

    // Unknown actions do not exist; the body names a reviewer and nothing else.
    let answer = send(
        &admin,
        Method::POST,
        "/v1/admin/templates/acme.shipped@1/publish",
        token,
        &[],
        Some(("application/json", br#"{"reviewer":"ops"}"#.to_vec())),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    for body in [
        r#"{"note":"no reviewer"}"#,
        r#"{"reviewer":"ops","extra":1}"#,
        "not json",
    ] {
        let answer = send(
            &admin,
            Method::POST,
            "/v1/admin/templates/acme.shipped@1/approve",
            token,
            &[],
            Some(("application/json", body.as_bytes().to_vec())),
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(answer.json()["error"]["code"], "invalid");
    }
    assert_eq!(registry.reviews.lock().unwrap().len(), 3);

    registry.refusing(AuthError::TemplateConflict(
        "acme.shipped@1 is not pending".to_owned(),
    ));
    let answer = send(
        &admin,
        Method::POST,
        "/v1/admin/templates/acme.shipped@1/approve",
        token,
        &[],
        Some(("application/json", br#"{"reviewer":"ops"}"#.to_vec())),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert_eq!(
        answer.json()["error"]["message"],
        "acme.shipped@1 is not pending"
    );
}
