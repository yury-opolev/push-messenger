//! The `Templates` service: auth-svc's template registry calls (spec 2026-10-26, rich
//! message templates, D5–D8 and D12), and seeding the built-ins at start-up.

use std::sync::Arc;

use push_domain::templates::{Definition, TemplateRef, builtins, outline};
use push_domain::{
    Clock, DomainError, DomainResult, Messaging, Notifier, ReviewAction, ReviewCommand, Store,
    StoredTemplate, TemplateEvent, TemplateStatus, UserId,
};
use push_proto::internal::v1::{
    self as internal, CatalogEntry, CatalogRequest, CatalogResponse, GetTemplateRequest,
    ListTemplatesRequest, ReviewTemplateRequest, SubmitTemplateRequest, TemplateDetail,
    TemplateList, TemplateSummary, templates_server,
};
use push_proto::v1::{self, ErrorCode};
use push_proto::{error_status, id_bytes};
use tonic::{Code, Request, Response, Status};

use crate::convert::{domain_error, invalid, micros, parse_id};

/// The `Templates` service. Guarded by auth-svc's internal token, as `Directory` is; never
/// logs a definition or a body.
pub struct TemplatesApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> TemplatesApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> templates_server::Templates for TemplatesApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn submit(
        &self,
        request: Request<SubmitTemplateRequest>,
    ) -> Result<Response<TemplateSummary>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let stored = self
            .messaging
            .submit_template(user, &request.definition)
            .await
            .map_err(submit_error)?;
        tracing::info!(
            target: "security",
            event = "templates.submitted",
            template = %stored.template,
            user = %user.0,
            "a template was submitted"
        );
        Ok(Response::new(summary(&stored)))
    }

    async fn catalog(
        &self,
        request: Request<CatalogRequest>,
    ) -> Result<Response<CatalogResponse>, Status> {
        let request = request.into_inner();
        let catalog = self
            .messaging
            .template_catalog(request.since, request.limit)
            .await
            .map_err(domain_error)?;
        let entries = catalog
            .entries
            .into_iter()
            .map(catalog_entry)
            .collect::<Result<_, _>>()?;
        Ok(Response::new(CatalogResponse {
            seq: catalog.seq,
            more: catalog.more,
            entries,
        }))
    }

    async fn list(
        &self,
        request: Request<ListTemplatesRequest>,
    ) -> Result<Response<TemplateList>, Status> {
        let request = request.into_inner();
        let status = from_status(request.status());
        let found = self
            .messaging
            .list_templates(status, request.limit)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(TemplateList {
            templates: found.iter().map(summary).collect(),
        }))
    }

    async fn get(
        &self,
        request: Request<GetTemplateRequest>,
    ) -> Result<Response<TemplateDetail>, Status> {
        let template = parse_ref(&request.into_inner().template_ref)?;
        let (stored, events) = self
            .messaging
            .template_detail(&template)
            .await
            .map_err(domain_error)?;
        // It passed every rule when it was submitted; failing now is a storage fault.
        let definition = Definition::parse(stored.definition.as_bytes()).map_err(|error| {
            domain_error(DomainError::Store(format!(
                "the stored definition of {template} is invalid: {error}"
            )))
        })?;
        Ok(Response::new(TemplateDetail {
            summary: Some(summary(&stored)),
            outline: outline(&definition),
            definition: stored.definition,
            events: events.into_iter().map(event).collect(),
        }))
    }

    async fn review(
        &self,
        request: Request<ReviewTemplateRequest>,
    ) -> Result<Response<TemplateSummary>, Status> {
        let request = request.into_inner();
        let action = match request.action() {
            internal::ReviewAction::Unspecified => return Err(invalid("action is required")),
            internal::ReviewAction::Approve => ReviewAction::Approve,
            internal::ReviewAction::Reject => ReviewAction::Reject,
            internal::ReviewAction::Revoke => ReviewAction::Revoke,
        };
        let template = parse_ref(&request.template_ref)?;
        let reviewer = request.reviewer;
        let stored = self
            .messaging
            .review_template(ReviewCommand {
                template,
                action,
                reviewer: reviewer.clone(),
                note: request.note,
            })
            .await
            .map_err(domain_error)?;
        let (event, message) = match action {
            ReviewAction::Approve => ("templates.approved", "a template was approved"),
            ReviewAction::Reject => ("templates.rejected", "a template was rejected"),
            ReviewAction::Revoke => ("templates.revoked", "a template was revoked"),
        };
        // The domain checked the reviewer label: printable, at most 64 characters.
        tracing::info!(
            target: "security",
            event,
            template = %stored.template,
            reviewer,
            "{message}"
        );
        Ok(Response::new(summary(&stored)))
    }
}

/// Stores the missing built-ins (see [`Messaging::seed_builtin_templates`]) and logs how
/// many it stored. Warns about a stored built-in whose definition differs from the shipped
/// one: a built-in edited without a new version, which clients that cached the old one
/// would render differently.
pub async fn seed_builtins<S, C, N>(messaging: &Messaging<S, C, N>) -> DomainResult<usize>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    let seeded = messaging.seed_builtin_templates().await?;
    if seeded > 0 {
        tracing::info!(
            target: "security",
            event = "templates.seeded",
            count = seeded,
            "built-in templates were stored"
        );
    }
    for builtin in builtins() {
        let template = builtin.template_ref();
        let (stored, _) = messaging.template_detail(&template).await?;
        if stored.definition != builtin.canonical_json() {
            tracing::warn!(
                target: "security",
                event = "templates.builtin_mismatch",
                template = %template,
                "a stored built-in template differs from the shipped one; give a changed \
                 built-in a new version"
            );
        }
    }
    Ok(seeded)
}

/// Whether seeding failed for a reason that waiting will not fix.
pub(crate) fn seeding_is_permanent(error: &DomainError) -> bool {
    !error.is_retryable() && !matches!(error, DomainError::Store(_))
}

/// A submission's failure. The worker relays new accounts to message-svc asynchronously,
/// so an account it does not know yet is about to exist: retryable, not not-found.
fn submit_error(error: DomainError) -> Status {
    match error {
        DomainError::UnknownUser => error_status(
            Code::Unavailable,
            v1::Error {
                code: ErrorCode::Unavailable.into(),
                message: "the account is not ready yet; retry shortly".to_owned(),
                retryable: true,
                client_msg_id: None,
                request_id: None,
                retry_after_s: None,
            },
        ),
        other => domain_error(other),
    }
}

fn parse_ref(template_ref: &str) -> Result<TemplateRef, Status> {
    template_ref
        .parse()
        .map_err(|error| invalid(format!("template_ref: {error}")))
}

fn summary(stored: &StoredTemplate) -> TemplateSummary {
    TemplateSummary {
        template_ref: stored.template.to_string(),
        status: to_status(stored.status).into(),
        submitted_by: stored.submitted_by.map(|user| id_bytes(user.0)),
        submitted_at: micros(stored.submitted_at),
        reviewed_at: stored.reviewed_at.map(micros),
        change_seq: stored.change_seq,
        title: stored.title.clone(),
    }
}

/// A catalog entry, carrying the definition only while the version is approved.
fn catalog_entry(stored: StoredTemplate) -> Result<CatalogEntry, Status> {
    let change_seq = stored.change_seq.ok_or_else(|| {
        domain_error(DomainError::Store(format!(
            "catalog entry {} has no change number",
            stored.template
        )))
    })?;
    let approved = stored.status == TemplateStatus::Approved;
    Ok(CatalogEntry {
        template_ref: stored.template.to_string(),
        status: to_status(stored.status).into(),
        change_seq,
        definition: approved.then_some(stored.definition),
    })
}

fn event(event: TemplateEvent) -> internal::TemplateEvent {
    internal::TemplateEvent {
        action: event.action.as_str().to_owned(),
        actor: event.actor.map(|user| id_bytes(user.0)),
        reviewer: event.reviewer,
        note: event.note,
        at: micros(event.at),
    }
}

fn to_status(status: TemplateStatus) -> internal::TemplateStatus {
    match status {
        TemplateStatus::Pending => internal::TemplateStatus::Pending,
        TemplateStatus::Approved => internal::TemplateStatus::Approved,
        TemplateStatus::Rejected => internal::TemplateStatus::Rejected,
        TemplateStatus::Revoked => internal::TemplateStatus::Revoked,
    }
}

/// A listing filter: `UNSPECIFIED` is every status.
fn from_status(status: internal::TemplateStatus) -> Option<TemplateStatus> {
    match status {
        internal::TemplateStatus::Unspecified => None,
        internal::TemplateStatus::Pending => Some(TemplateStatus::Pending),
        internal::TemplateStatus::Approved => Some(TemplateStatus::Approved),
        internal::TemplateStatus::Rejected => Some(TemplateStatus::Rejected),
        internal::TemplateStatus::Revoked => Some(TemplateStatus::Revoked),
    }
}

#[cfg(test)]
mod tests {
    use push_domain::{InMemoryStore, NoopNotifier, PublisherOwner, SystemClock, Tx};
    use push_proto::error_from_status;
    use push_proto::internal::v1::templates_server::Templates as TemplatesRpc;
    use push_proto::v1::ErrorCode;
    use tonic::Code;

    use super::*;

    type Api = TemplatesApi<InMemoryStore, SystemClock, NoopNotifier>;

    fn api() -> (
        Api,
        Arc<Messaging<InMemoryStore, SystemClock, NoopNotifier>>,
    ) {
        let messaging = Arc::new(Messaging::new(
            InMemoryStore::default(),
            SystemClock,
            NoopNotifier,
        ));
        (TemplatesApi::new(Arc::clone(&messaging)), messaging)
    }

    /// A tpl/1 definition of `acme.<name>@1` open to people.
    fn definition(name: &str) -> Vec<u8> {
        format!(
            r#"{{"format":"tpl/1","publisher":"acme","name":"{name}","version":1,
            "title":"Deploy check","senders":["human"],
            "params":{{"text":{{"type":"string","max_len":200}}}},
            "layout":[{{"type":"text","param":"text"}}]}}"#
        )
        .into_bytes()
    }

    async fn submit(api: &Api, user: UserId, name: &str) -> TemplateSummary {
        api.submit(Request::new(SubmitTemplateRequest {
            user_id: id_bytes(user.0),
            definition: definition(name),
        }))
        .await
        .unwrap()
        .into_inner()
    }

    fn review_request(
        template_ref: &str,
        action: internal::ReviewAction,
    ) -> Request<ReviewTemplateRequest> {
        Request::new(ReviewTemplateRequest {
            template_ref: template_ref.to_owned(),
            action: action.into(),
            reviewer: "ops".to_owned(),
            note: Some("looks fine".to_owned()),
        })
    }

    async fn catalog(api: &Api, since: i64) -> CatalogResponse {
        api.catalog(Request::new(CatalogRequest { since, limit: 0 }))
            .await
            .unwrap()
            .into_inner()
    }

    #[tokio::test]
    async fn an_approved_submission_enters_the_catalog_with_its_definition() {
        let (api, messaging) = api();
        let user = UserId::new();
        messaging.register_user(user).await.unwrap();

        let submitted = submit(&api, user, "deploy").await;
        assert_eq!(submitted.template_ref, "acme.deploy@1");
        assert_eq!(submitted.status(), internal::TemplateStatus::Pending);
        assert_eq!(submitted.submitted_by, Some(id_bytes(user.0)));
        assert_eq!(submitted.title, "Deploy check");
        assert_eq!((submitted.reviewed_at, submitted.change_seq), (None, None));
        assert!(catalog(&api, 0).await.entries.is_empty());

        let approved = api
            .review(review_request(
                "acme.deploy@1",
                internal::ReviewAction::Approve,
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(approved.status(), internal::TemplateStatus::Approved);
        assert_eq!(approved.change_seq, Some(1));
        assert!(approved.reviewed_at.is_some());

        let page = catalog(&api, 0).await;
        assert_eq!((page.seq, page.more), (1, false));
        let canonical = Definition::parse(&definition("deploy"))
            .unwrap()
            .canonical_json();
        assert_eq!(
            page.entries,
            [CatalogEntry {
                template_ref: "acme.deploy@1".to_owned(),
                status: internal::TemplateStatus::Approved.into(),
                change_seq: 1,
                definition: Some(canonical),
            }]
        );

        // A revoked version stays in the catalog without its definition.
        api.review(review_request(
            "acme.deploy@1",
            internal::ReviewAction::Revoke,
        ))
        .await
        .unwrap();
        let page = catalog(&api, 1).await;
        assert_eq!(page.seq, 2);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].status(), internal::TemplateStatus::Revoked);
        assert_eq!(page.entries[0].definition, None);
    }

    #[tokio::test]
    async fn get_returns_the_definition_its_outline_and_the_audit_trail() {
        let (api, messaging) = api();
        let user = UserId::new();
        messaging.register_user(user).await.unwrap();
        submit(&api, user, "deploy").await;
        api.review(review_request(
            "acme.deploy@1",
            internal::ReviewAction::Reject,
        ))
        .await
        .unwrap();

        let detail = api
            .get(Request::new(GetTemplateRequest {
                template_ref: "acme.deploy@1".to_owned(),
            }))
            .await
            .unwrap()
            .into_inner();
        let parsed = Definition::parse(&definition("deploy")).unwrap();
        assert_eq!(detail.definition, parsed.canonical_json());
        assert_eq!(detail.outline, outline(&parsed));
        assert!(!detail.outline.is_empty());
        let summary = detail.summary.unwrap();
        assert_eq!(summary.status(), internal::TemplateStatus::Rejected);
        assert_eq!(
            detail.events,
            [
                internal::TemplateEvent {
                    action: "submit".to_owned(),
                    actor: Some(id_bytes(user.0)),
                    reviewer: None,
                    note: None,
                    at: summary.submitted_at,
                },
                internal::TemplateEvent {
                    action: "reject".to_owned(),
                    actor: None,
                    reviewer: Some("ops".to_owned()),
                    note: Some("looks fine".to_owned()),
                    at: summary.reviewed_at.unwrap(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn malformed_and_unknown_refs_and_actions_are_refused() {
        let (api, _messaging) = api();
        let get = |template_ref: &str| {
            Request::new(GetTemplateRequest {
                template_ref: template_ref.to_owned(),
            })
        };
        let status = api.get(get("not a ref")).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(
            error_from_status(&status)
                .message
                .starts_with("template_ref")
        );
        let status = api.get(get("acme.unknown@1")).await.unwrap_err();
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(error_from_status(&status).code(), ErrorCode::Rejected);

        let status = api
            .review(review_request(
                "acme.deploy@1",
                internal::ReviewAction::Unspecified,
            ))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(error_from_status(&status).message, "action is required");
        let status = api
            .review(review_request("acme..x", internal::ReviewAction::Approve))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        let status = api
            .submit(Request::new(SubmitTemplateRequest {
                user_id: vec![1, 2, 3],
                definition: definition("deploy"),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_submission_by_an_account_not_relayed_yet_is_retryable() {
        let (api, _messaging) = api();
        let status = api
            .submit(Request::new(SubmitTemplateRequest {
                user_id: id_bytes(UserId::new().0),
                definition: definition("deploy"),
            }))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unavailable);
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(error.retryable);
        assert_eq!(error.message, "the account is not ready yet; retry shortly");
    }

    #[tokio::test]
    async fn listings_filter_by_status_and_unspecified_lists_all() {
        let (api, messaging) = api();
        let user = UserId::new();
        messaging.register_user(user).await.unwrap();
        submit(&api, user, "first").await;
        submit(&api, user, "second").await;
        api.review(review_request(
            "acme.second@1",
            internal::ReviewAction::Approve,
        ))
        .await
        .unwrap();
        let list = |status: internal::TemplateStatus| {
            api.list(Request::new(ListTemplatesRequest {
                status: status.into(),
                limit: 0,
            }))
        };
        let refs = |list: TemplateList| -> Vec<String> {
            let mut refs: Vec<String> =
                list.templates.into_iter().map(|t| t.template_ref).collect();
            refs.sort();
            refs
        };
        assert_eq!(
            refs(
                list(internal::TemplateStatus::Unspecified)
                    .await
                    .unwrap()
                    .into_inner()
            ),
            ["acme.first@1", "acme.second@1"]
        );
        assert_eq!(
            refs(
                list(internal::TemplateStatus::Pending)
                    .await
                    .unwrap()
                    .into_inner()
            ),
            ["acme.first@1"]
        );
        assert_eq!(
            refs(
                list(internal::TemplateStatus::Approved)
                    .await
                    .unwrap()
                    .into_inner()
            ),
            ["acme.second@1"]
        );
    }

    #[tokio::test]
    async fn seeding_stores_the_builtins_once_and_the_catalog_lists_them() {
        let (api, messaging) = api();
        assert_eq!(seed_builtins(&messaging).await, Ok(4));
        assert_eq!(seed_builtins(&messaging).await, Ok(0));
        let page = catalog(&api, 0).await;
        let mut refs: Vec<&str> = page
            .entries
            .iter()
            .map(|entry| entry.template_ref.as_str())
            .collect();
        refs.sort();
        assert_eq!(
            refs,
            [
                "agent.approval@1",
                "agent.plan@1",
                "agent.question@1",
                "agent.reply@1"
            ]
        );
        assert!(page.entries.iter().all(|entry| entry.definition.is_some()));
    }

    #[tokio::test]
    async fn seeding_fails_for_good_when_an_account_holds_the_builtin_publisher() {
        let store = InMemoryStore::default();
        let messaging = Messaging::new(store.clone(), SystemClock, NoopNotifier);
        let user = UserId::new();
        messaging.register_user(user).await.unwrap();
        let mut tx = store.begin().await.unwrap();
        tx.claim_publisher("agent", PublisherOwner::User(user))
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let error = seed_builtins(&messaging).await.unwrap_err();
        assert_eq!(
            error,
            DomainError::TemplateConflict("the built-in publisher belongs to an account")
        );
        assert!(seeding_is_permanent(&error));
    }

    #[test]
    fn only_a_retryable_or_storage_failure_lets_seeding_retry() {
        assert!(!seeding_is_permanent(&DomainError::Conflict));
        assert!(!seeding_is_permanent(&DomainError::Store("down".into())));
        assert!(seeding_is_permanent(&DomainError::TemplateExists));
    }
}
