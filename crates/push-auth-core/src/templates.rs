//! The template registry (spec 2026-10-26 rich message templates, D4–D8): message-svc
//! keeps it, and auth-svc fronts it over HTTP. auth-svc calls message-svc's internal
//! `Templates` API with the same internal token and channel as `Directory`.
//!
//! message-svc's refusals keep their human message: a definition that breaks a rule is
//! `TemplateInvalid` (400 `invalid`), an unknown ref `TemplateNotFound` (404 `not_found`),
//! an existing ref, a status that does not allow the review, another account's publisher
//! or too many pending submissions `TemplateConflict` (409 `conflict`). Anything else is
//! `Unavailable`.

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use push_proto::internal::v1 as internal;
use push_proto::internal::v1::templates_client::TemplatesClient;
use push_proto::{error_from_status, id, id_bytes};
use serde_json::{Value, json};
use tonic::{Code, Status};
use uuid::Uuid;

use crate::directory::{MessageSvc, message_svc};
use crate::error::AuthError;

/// The longest message of message-svc's that is passed on to the client.
const MAX_MESSAGE_CHARS: usize = 512;

/// Where a version stands (spec D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateStatus {
    Pending,
    Approved,
    Rejected,
    Revoked,
}

impl TemplateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Revoked => "revoked",
        }
    }

    /// The status named `name`, as [`Self::as_str`] writes it.
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Pending, Self::Approved, Self::Rejected, Self::Revoked]
            .into_iter()
            .find(|status| status.as_str() == name)
    }

    fn to_proto(self) -> internal::TemplateStatus {
        match self {
            Self::Pending => internal::TemplateStatus::Pending,
            Self::Approved => internal::TemplateStatus::Approved,
            Self::Rejected => internal::TemplateStatus::Rejected,
            Self::Revoked => internal::TemplateStatus::Revoked,
        }
    }

    fn from_proto(status: internal::TemplateStatus) -> Result<Self, AuthError> {
        match status {
            internal::TemplateStatus::Pending => Ok(Self::Pending),
            internal::TemplateStatus::Approved => Ok(Self::Approved),
            internal::TemplateStatus::Rejected => Ok(Self::Rejected),
            internal::TemplateStatus::Revoked => Ok(Self::Revoked),
            internal::TemplateStatus::Unspecified => Err(malformed("a status")),
        }
    }
}

/// What a review does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    /// A pending version becomes approved.
    Approve,
    /// A pending version becomes rejected.
    Reject,
    /// An approved version becomes revoked.
    Revoke,
}

impl ReviewAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Revoke => "revoke",
        }
    }

    fn to_proto(self) -> internal::ReviewAction {
        match self {
            Self::Approve => internal::ReviewAction::Approve,
            Self::Reject => internal::ReviewAction::Reject,
            Self::Revoke => internal::ReviewAction::Revoke,
        }
    }
}

/// One version, without its definition.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateSummary {
    /// `<publisher>.<name>@<version>`.
    pub template: String,
    pub status: TemplateStatus,
    pub title: String,
    /// `None` for a built-in.
    pub submitted_by: Option<Uuid>,
    pub submitted_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
    /// The change number, once approved, revoked or seeded.
    pub seq: Option<i64>,
}

/// One change of the client catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub template: String,
    /// Approved or revoked.
    pub status: TemplateStatus,
    pub seq: i64,
    /// Approved versions only: the canonical definition.
    pub definition: Option<Value>,
}

/// A page of the client catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    /// The change number to ask from next.
    pub seq: i64,
    /// More changes wait beyond this page.
    pub more: bool,
    pub entries: Vec<CatalogEntry>,
}

/// One row of a version's audit trail.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateEvent {
    /// submit, approve, reject, revoke or seed.
    pub action: String,
    pub actor: Option<Uuid>,
    pub reviewer: Option<String>,
    pub note: Option<String>,
    pub at: DateTime<Utc>,
}

/// One version for review.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateDetail {
    pub summary: TemplateSummary,
    /// The canonical definition.
    pub definition: Value,
    /// A plain-text rendering of the layout.
    pub outline: String,
    /// Oldest first.
    pub events: Vec<TemplateEvent>,
}

/// message-svc's template registry.
#[async_trait]
pub trait TemplateRegistry: Send + Sync + std::fmt::Debug {
    /// Stores `definition`, the client's JSON exactly as sent, as a pending version
    /// submitted by `user`.
    async fn submit(&self, user: Uuid, definition: Vec<u8>) -> Result<TemplateSummary, AuthError>;

    /// Approved and revoked versions changed after `since`, oldest change first; at most
    /// `limit` (0: the server's maximum).
    async fn catalog(&self, since: i64, limit: u32) -> Result<Catalog, AuthError>;

    /// Versions with `status` (`None`: every status), newest submission first; at most
    /// `limit` (0: the server's maximum).
    async fn list(
        &self,
        status: Option<TemplateStatus>,
        limit: u32,
    ) -> Result<Vec<TemplateSummary>, AuthError>;

    /// One version with its definition, outline and audit trail.
    async fn get(&self, template: &str) -> Result<TemplateDetail, AuthError>;

    /// Approves, rejects or revokes `template` in the name of `reviewer`.
    async fn review(
        &self,
        template: &str,
        action: ReviewAction,
        reviewer: &str,
        note: Option<&str>,
    ) -> Result<TemplateSummary, AuthError>;
}

/// message-svc's `Templates` API over gRPC.
#[derive(Debug, Clone)]
pub struct GrpcTemplates {
    client: TemplatesClient<MessageSvc>,
}

impl GrpcTemplates {
    /// A client for message-svc's Templates API at `url` presenting `token`, auth-svc's
    /// internal token (the Directory's). It connects on first use.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        Ok(Self::over(message_svc(url, token)?))
    }

    /// A client over an existing channel to message-svc.
    pub(crate) fn over(channel: MessageSvc) -> Self {
        Self {
            client: TemplatesClient::new(channel),
        }
    }
}

#[async_trait]
impl TemplateRegistry for GrpcTemplates {
    async fn submit(&self, user: Uuid, definition: Vec<u8>) -> Result<TemplateSummary, AuthError> {
        let request = internal::SubmitTemplateRequest {
            user_id: id_bytes(user),
            definition,
        };
        let response = self
            .client
            .clone()
            .submit(request)
            .await
            .map_err(|status| status_error("Submit", &status))?;
        summary(response.into_inner())
    }

    async fn catalog(&self, since: i64, limit: u32) -> Result<Catalog, AuthError> {
        let response = self
            .client
            .clone()
            .catalog(internal::CatalogRequest { since, limit })
            .await
            .map_err(|status| status_error("Catalog", &status))?
            .into_inner();
        let entries = response
            .entries
            .into_iter()
            .map(|entry| {
                Ok(CatalogEntry {
                    status: TemplateStatus::from_proto(entry.status())?,
                    definition: entry.definition.as_deref().map(definition).transpose()?,
                    template: entry.template_ref,
                    seq: entry.change_seq,
                })
            })
            .collect::<Result<_, AuthError>>()?;
        Ok(Catalog {
            seq: response.seq,
            more: response.more,
            entries,
        })
    }

    async fn list(
        &self,
        status: Option<TemplateStatus>,
        limit: u32,
    ) -> Result<Vec<TemplateSummary>, AuthError> {
        let status = status.map_or(internal::TemplateStatus::Unspecified, |status| {
            status.to_proto()
        });
        let response = self
            .client
            .clone()
            .list(internal::ListTemplatesRequest {
                status: status.into(),
                limit,
            })
            .await
            .map_err(|status| status_error("List", &status))?;
        response
            .into_inner()
            .templates
            .into_iter()
            .map(summary)
            .collect()
    }

    async fn get(&self, template: &str) -> Result<TemplateDetail, AuthError> {
        let response = self
            .client
            .clone()
            .get(internal::GetTemplateRequest {
                template_ref: template.to_owned(),
            })
            .await
            .map_err(|status| status_error("Get", &status))?
            .into_inner();
        let events = response
            .events
            .into_iter()
            .map(|event| {
                Ok(TemplateEvent {
                    actor: event.actor.as_deref().map(user).transpose()?,
                    at: time(event.at)?,
                    action: event.action,
                    reviewer: event.reviewer,
                    note: event.note,
                })
            })
            .collect::<Result<_, AuthError>>()?;
        Ok(TemplateDetail {
            summary: summary(response.summary.ok_or_else(|| malformed("a summary"))?)?,
            definition: definition(&response.definition)?,
            outline: response.outline,
            events,
        })
    }

    async fn review(
        &self,
        template: &str,
        action: ReviewAction,
        reviewer: &str,
        note: Option<&str>,
    ) -> Result<TemplateSummary, AuthError> {
        let response = self
            .client
            .clone()
            .review(internal::ReviewTemplateRequest {
                template_ref: template.to_owned(),
                action: action.to_proto().into(),
                reviewer: reviewer.to_owned(),
                note: note.map(str::to_owned),
            })
            .await
            .map_err(|status| status_error("Review", &status))?;
        summary(response.into_inner())
    }
}

/// Without message-svc: every call is `Unavailable`. For an auth-svc started without
/// `PUSH_MESSAGE_SVC_URL` or `PUSH_DIRECTORY_TOKEN`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTemplates;

#[async_trait]
impl TemplateRegistry for NoTemplates {
    async fn submit(&self, _: Uuid, _: Vec<u8>) -> Result<TemplateSummary, AuthError> {
        Err(AuthError::Unavailable)
    }

    async fn catalog(&self, _: i64, _: u32) -> Result<Catalog, AuthError> {
        Err(AuthError::Unavailable)
    }

    async fn list(
        &self,
        _: Option<TemplateStatus>,
        _: u32,
    ) -> Result<Vec<TemplateSummary>, AuthError> {
        Err(AuthError::Unavailable)
    }

    async fn get(&self, _: &str) -> Result<TemplateDetail, AuthError> {
        Err(AuthError::Unavailable)
    }

    async fn review(
        &self,
        _: &str,
        _: ReviewAction,
        _: &str,
        _: Option<&str>,
    ) -> Result<TemplateSummary, AuthError> {
        Err(AuthError::Unavailable)
    }
}

/// The error a refusal of message-svc's stands for; its message, carried as a
/// `push.v1.Error` in the details, is kept for the client.
pub(crate) fn status_error(rpc: &'static str, status: &Status) -> AuthError {
    let message = || {
        let message = if status.details().is_empty() {
            status.message().to_owned()
        } else {
            error_from_status(status).message
        };
        let message = message.trim();
        if message.is_empty() {
            "the template registry refused the request".to_owned()
        } else {
            message.chars().take(MAX_MESSAGE_CHARS).collect()
        }
    };
    match status.code() {
        Code::InvalidArgument => AuthError::TemplateInvalid(message()),
        Code::NotFound => AuthError::TemplateNotFound,
        Code::AlreadyExists | Code::FailedPrecondition => AuthError::TemplateConflict(message()),
        code => {
            tracing::warn!(?code, rpc, "message-svc's Templates API did not answer");
            AuthError::Unavailable
        }
    }
}

/// An answer of message-svc's that breaks the contract, logged; the request is
/// `Unavailable`.
fn malformed(what: &str) -> AuthError {
    tracing::warn!(
        what,
        "message-svc's Templates API answered with a malformed value"
    );
    AuthError::Unavailable
}

fn summary(summary: internal::TemplateSummary) -> Result<TemplateSummary, AuthError> {
    Ok(TemplateSummary {
        status: TemplateStatus::from_proto(summary.status())?,
        submitted_by: summary.submitted_by.as_deref().map(user).transpose()?,
        submitted_at: time(summary.submitted_at)?,
        reviewed_at: summary.reviewed_at.map(time).transpose()?,
        template: summary.template_ref,
        title: summary.title,
        seq: summary.change_seq,
    })
}

fn user(bytes: &[u8]) -> Result<Uuid, AuthError> {
    id(bytes).map_err(|_| malformed("a user id"))
}

fn time(micros: i64) -> Result<DateTime<Utc>, AuthError> {
    DateTime::from_timestamp_micros(micros).ok_or_else(|| malformed("a time"))
}

/// A canonical definition as JSON: an object, or the answer is malformed.
fn definition(text: &str) -> Result<Value, AuthError> {
    match serde_json::from_str(text) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(malformed("a definition")),
    }
}

/// A time as the API writes it: RFC 3339 in UTC, with milliseconds.
pub(crate) fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl TemplateSummary {
    /// The summary as the admin API answers it.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "template": self.template,
            "status": self.status.as_str(),
            "title": self.title,
            "submitted_by": self.submitted_by,
            "submitted_at": rfc3339(self.submitted_at),
            "reviewed_at": self.reviewed_at.map(rfc3339),
            "seq": self.seq,
        })
    }
}

impl TemplateDetail {
    /// The version as the admin API answers it: the summary's fields, the definition, the
    /// outline and the audit trail.
    pub(crate) fn to_json(&self) -> Value {
        let mut answer = self.summary.to_json();
        let events: Vec<Value> = self
            .events
            .iter()
            .map(|event| {
                json!({
                    "action": event.action,
                    "actor": event.actor,
                    "reviewer": event.reviewer,
                    "note": event.note,
                    "at": rfc3339(event.at),
                })
            })
            .collect();
        if let Value::Object(fields) = &mut answer {
            fields.insert("definition".to_owned(), self.definition.clone());
            fields.insert("outline".to_owned(), Value::String(self.outline.clone()));
            fields.insert("events".to_owned(), Value::Array(events));
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use push_proto::internal::v1::templates_server::{self, TemplatesServer};
    use push_proto::v1::{Error, ErrorCode};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request, Response};

    use super::*;

    const TOKEN: &str = "directory-token-for-the-template-tests";

    /// A message-svc that checks the token, keeps what it was sent and answers fixed values.
    #[derive(Default, Clone)]
    struct Registry {
        submitted: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    fn checked<T>(request: &Request<T>) -> Result<(), Status> {
        let presented = request.metadata().get("authorization");
        if presented.and_then(|value| value.to_str().ok()) != Some(&format!("Bearer {TOKEN}")) {
            return Err(Status::unauthenticated("wrong token"));
        }
        Ok(())
    }

    fn proto_summary() -> internal::TemplateSummary {
        internal::TemplateSummary {
            template_ref: "acme.shipped@1".to_owned(),
            status: internal::TemplateStatus::Approved.into(),
            submitted_by: Some(id_bytes(Uuid::from_u128(7))),
            submitted_at: 1_000_000,
            reviewed_at: Some(2_000_000),
            change_seq: Some(4),
            title: "Shipped".to_owned(),
        }
    }

    #[tonic::async_trait]
    impl templates_server::Templates for Registry {
        async fn submit(
            &self,
            request: Request<internal::SubmitTemplateRequest>,
        ) -> Result<Response<internal::TemplateSummary>, Status> {
            checked(&request)?;
            let request = request.into_inner();
            if request.definition == b"bad" {
                return Err(push_proto::error_status(
                    Code::InvalidArgument,
                    Error {
                        code: ErrorCode::Rejected.into(),
                        message: "title: 1 to 80 characters".to_owned(),
                        retryable: false,
                        client_msg_id: None,
                        request_id: None,
                        retry_after_s: None,
                    },
                ));
            }
            self.submitted.lock().unwrap().push(request.definition);
            Ok(Response::new(proto_summary()))
        }

        async fn catalog(
            &self,
            request: Request<internal::CatalogRequest>,
        ) -> Result<Response<internal::CatalogResponse>, Status> {
            checked(&request)?;
            Ok(Response::new(internal::CatalogResponse {
                seq: 4,
                more: true,
                entries: vec![internal::CatalogEntry {
                    template_ref: "acme.shipped@1".to_owned(),
                    status: internal::TemplateStatus::Approved.into(),
                    change_seq: 4,
                    definition: Some(r#"{"title":"Shipped"}"#.to_owned()),
                }],
            }))
        }

        async fn list(
            &self,
            request: Request<internal::ListTemplatesRequest>,
        ) -> Result<Response<internal::TemplateList>, Status> {
            checked(&request)?;
            Err(Status::failed_precondition("too many pending"))
        }

        async fn get(
            &self,
            request: Request<internal::GetTemplateRequest>,
        ) -> Result<Response<internal::TemplateDetail>, Status> {
            checked(&request)?;
            Err(Status::not_found("template not found"))
        }

        async fn review(
            &self,
            request: Request<internal::ReviewTemplateRequest>,
        ) -> Result<Response<internal::TemplateSummary>, Status> {
            checked(&request)?;
            Err(Status::already_exists("acme.shipped@1 exists"))
        }
    }

    async fn message_svc(registry: Registry) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(TemplatesServer::new(registry))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        url
    }

    #[tokio::test]
    async fn calls_carry_the_token_and_refusals_keep_their_meaning() {
        let registry = Registry::default();
        let url = message_svc(registry.clone()).await;
        let templates = GrpcTemplates::new(&url, TOKEN).unwrap();
        assert!(!format!("{templates:?}").contains(TOKEN));

        let submitted = templates
            .submit(Uuid::from_u128(1), b"{ \"raw\" : 1 }".to_vec())
            .await
            .unwrap();
        assert_eq!(
            submitted,
            TemplateSummary {
                template: "acme.shipped@1".to_owned(),
                status: TemplateStatus::Approved,
                title: "Shipped".to_owned(),
                submitted_by: Some(Uuid::from_u128(7)),
                submitted_at: DateTime::from_timestamp(1, 0).unwrap(),
                reviewed_at: Some(DateTime::from_timestamp(2, 0).unwrap()),
                seq: Some(4),
            }
        );
        assert_eq!(
            *registry.submitted.lock().unwrap(),
            vec![b"{ \"raw\" : 1 }".to_vec()]
        );
        assert_eq!(
            templates.submit(Uuid::from_u128(1), b"bad".to_vec()).await,
            Err(AuthError::TemplateInvalid(
                "title: 1 to 80 characters".to_owned()
            ))
        );
        let catalog = templates.catalog(0, 0).await.unwrap();
        assert_eq!((catalog.seq, catalog.more), (4, true));
        assert_eq!(
            catalog.entries[0].definition,
            Some(json!({"title": "Shipped"}))
        );
        assert_eq!(
            templates.list(None, 0).await,
            Err(AuthError::TemplateConflict("too many pending".to_owned()))
        );
        assert_eq!(
            templates.get("acme.shipped@1").await,
            Err(AuthError::TemplateNotFound)
        );
        assert_eq!(
            templates
                .review("acme.shipped@1", ReviewAction::Approve, "ops", None)
                .await,
            Err(AuthError::TemplateConflict(
                "acme.shipped@1 exists".to_owned()
            ))
        );

        let wrong = GrpcTemplates::new(&url, "another-token-of-the-right-length").unwrap();
        assert_eq!(wrong.catalog(0, 0).await, Err(AuthError::Unavailable));
        let unreachable = GrpcTemplates::new("http://127.0.0.1:1", TOKEN).unwrap();
        assert_eq!(unreachable.catalog(0, 0).await, Err(AuthError::Unavailable));
    }
}
