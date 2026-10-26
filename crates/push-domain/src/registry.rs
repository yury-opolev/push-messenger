//! The template registry: submissions, reviews, the client catalog and the built-ins
//! (spec 2026-10-26, rich message templates, D5–D8, D12 and D13).

use chrono::{DateTime, SubsecRound, Utc};

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::{PublisherOwner, StoredTemplate, TemplateAction, TemplateEvent, TemplateStatus};
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{InsertOutcome, Store, Tx};
use crate::templates::{BUILTIN_PUBLISHER, Definition, RESERVED_PUBLISHERS, TemplateRef, builtins};
use crate::validation::ValidationError;

/// Most submissions one account may have waiting for review.
pub const MAX_PENDING_PER_USER: u32 = 20;
/// Largest catalog page, and largest listing.
pub const MAX_CATALOG_PAGE: u32 = 200;
/// Longest reviewer label, in characters.
const MAX_REVIEWER_CHARS: usize = 64;
/// Longest review note, in characters.
const MAX_NOTE_CHARS: usize = 500;

/// What a reviewer does to a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    /// `pending → approved`: the version enters the catalog.
    Approve,
    /// `pending → rejected`: the version never enters the catalog.
    Reject,
    /// `approved → revoked`: the catalog lists the version as revoked.
    Revoke,
}

/// A review by a holder of the admin token (spec D5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCommand {
    pub template: TemplateRef,
    pub action: ReviewAction,
    /// Who reviewed, for the audit trail: 1 to 64 printable characters, not only spaces.
    pub reviewer: String,
    /// At most 500 characters; line breaks are the only control characters allowed.
    pub note: Option<String>,
}

/// A page of the client catalog (spec D8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// The position to ask from next.
    pub seq: i64,
    /// Whether more changes follow `seq` already.
    pub more: bool,
    /// Approved and revoked versions changed after the position asked from, oldest change
    /// first.
    pub entries: Vec<StoredTemplate>,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Stores `definition` as a pending version submitted by `user` (spec D6, D7).
    ///
    /// The first submission under a publisher claims it for `user`; later ones must come
    /// from the same account. Reserved publishers are refused, an account may have at most
    /// [`MAX_PENDING_PER_USER`] versions waiting, and a ref that is already stored, in any
    /// status, is `TemplateExists`.
    pub async fn submit_template(
        &self,
        user: UserId,
        definition: &[u8],
    ) -> DomainResult<StoredTemplate> {
        let definition = Definition::parse(definition)?;
        let template = definition.template_ref();
        if RESERVED_PUBLISHERS.contains(&template.publisher.as_str()) {
            return Err(DomainError::TemplateConflict("the publisher is reserved"));
        }
        let now = self.template_now();
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(user).await? {
            return Err(DomainError::UnknownUser);
        }
        // The user's lock serialises their submissions, whatever their publishers, so two
        // of them cannot both pass the pending cap. It comes before the publisher lock.
        tx.lock_users(&[user]).await?;
        let owner = PublisherOwner::User(user);
        if tx.claim_publisher(&template.publisher, owner).await? != owner {
            return Err(DomainError::TemplateConflict(
                "the publisher belongs to another account",
            ));
        }
        if tx.pending_templates_of(user).await? >= MAX_PENDING_PER_USER {
            return Err(DomainError::TemplateConflict(
                "too many submissions are waiting for review",
            ));
        }
        let stored = StoredTemplate {
            template: template.clone(),
            title: definition.title().to_owned(),
            definition: definition.canonical_json(),
            status: TemplateStatus::Pending,
            submitted_by: Some(user),
            submitted_at: now,
            reviewed_at: None,
            change_seq: None,
        };
        if tx.insert_template(stored.clone()).await? == InsertOutcome::Duplicate {
            return Err(DomainError::TemplateExists);
        }
        tx.insert_template_event(TemplateEvent {
            template,
            action: TemplateAction::Submit,
            actor: Some(user),
            reviewer: None,
            note: None,
            at: now,
        })
        .await?;
        tx.commit().await?;
        Ok(stored)
    }

    /// Approves, rejects or revokes a version (spec D5, D7, D8) and returns it as stored
    /// now. Approve and Reject need a pending version, Revoke an approved one; approving
    /// and revoking take the catalog's next change number.
    pub async fn review_template(&self, command: ReviewCommand) -> DomainResult<StoredTemplate> {
        validate_reviewer(&command.reviewer)?;
        if let Some(note) = &command.note {
            validate_note(note)?;
        }
        let (from, to, refusal) = match command.action {
            ReviewAction::Approve => (
                TemplateStatus::Pending,
                TemplateStatus::Approved,
                "the template is not pending",
            ),
            ReviewAction::Reject => (
                TemplateStatus::Pending,
                TemplateStatus::Rejected,
                "the template is not pending",
            ),
            ReviewAction::Revoke => (
                TemplateStatus::Approved,
                TemplateStatus::Revoked,
                "the template is not approved",
            ),
        };
        let now = self.template_now();
        let mut tx = self.store.begin().await?;
        let mut stored = tx
            .lock_template(&command.template)
            .await?
            .ok_or(DomainError::TemplateNotFound)?;
        if stored.status != from {
            return Err(DomainError::TemplateConflict(refusal));
        }
        let change_seq = match to {
            TemplateStatus::Rejected => None,
            _ => Some(tx.next_template_change().await?),
        };
        tx.set_template_status(&command.template, to, now, change_seq)
            .await?;
        tx.insert_template_event(TemplateEvent {
            template: command.template,
            action: match command.action {
                ReviewAction::Approve => TemplateAction::Approve,
                ReviewAction::Reject => TemplateAction::Reject,
                ReviewAction::Revoke => TemplateAction::Revoke,
            },
            actor: None,
            reviewer: Some(command.reviewer),
            note: command.note,
            at: now,
        })
        .await?;
        tx.commit().await?;
        stored.status = to;
        stored.reviewed_at = Some(now);
        stored.change_seq = change_seq;
        Ok(stored)
    }

    /// The approved and revoked versions changed after `since`, at most `limit` (0, or
    /// more than [`MAX_CATALOG_PAGE`], asks for [`MAX_CATALOG_PAGE`]). Asking again from
    /// the returned `seq` continues without gaps or repeats.
    pub async fn template_catalog(&self, since: i64, limit: u32) -> DomainResult<Catalog> {
        if since < 0 {
            return Err(ValidationError::CatalogPosition { actual: since }.into());
        }
        let limit = page_limit(limit);
        let mut tx = self.store.begin().await?;
        // Read the last change first: every change up to it has committed (change numbers
        // commit in order), so the entries read next include all of them, and `seq` never
        // passes a change the page did not see.
        let last = tx.last_template_change().await?;
        let entries = tx.templates_changed_since(since, limit).await?;
        tx.rollback().await?;
        let newest = entries
            .last()
            .and_then(|stored| stored.change_seq)
            .unwrap_or(since);
        let more = entries.len() == limit as usize;
        let seq = if more {
            newest
        } else {
            since.max(last).max(newest)
        };
        Ok(Catalog { seq, more, entries })
    }

    /// Versions with `status` (every version for `None`), newest submission first, at
    /// most `limit` (0, or more than [`MAX_CATALOG_PAGE`], asks for [`MAX_CATALOG_PAGE`]).
    pub async fn list_templates(
        &self,
        status: Option<TemplateStatus>,
        limit: u32,
    ) -> DomainResult<Vec<StoredTemplate>> {
        let mut tx = self.store.begin().await?;
        let found = tx.templates_with_status(status, page_limit(limit)).await?;
        tx.rollback().await?;
        Ok(found)
    }

    /// A version and its audit trail, oldest event first.
    pub async fn template_detail(
        &self,
        template: &TemplateRef,
    ) -> DomainResult<(StoredTemplate, Vec<TemplateEvent>)> {
        let mut tx = self.store.begin().await?;
        let stored = tx
            .template(template)
            .await?
            .ok_or(DomainError::TemplateNotFound)?;
        let events = tx.template_events(template).await?;
        tx.rollback().await?;
        Ok((stored, events))
    }

    /// Stores the built-ins that are missing as approved versions owned by the server
    /// (spec D12) and returns how many it stored. A built-in already stored, in any status,
    /// is left alone, so a revocation survives restarts. Idempotent, also when several
    /// instances seed at once.
    pub async fn seed_builtin_templates(&self) -> DomainResult<usize> {
        let mut inserted = 0;
        for definition in builtins() {
            if self.seed_builtin(&definition).await? {
                inserted += 1;
            }
        }
        Ok(inserted)
    }

    /// Stores one built-in unless it is stored; returns whether it stored it.
    async fn seed_builtin(&self, definition: &Definition) -> DomainResult<bool> {
        let template = definition.template_ref();
        let now = self.template_now();
        let mut tx = self.store.begin().await?;
        let owner = tx
            .claim_publisher(BUILTIN_PUBLISHER, PublisherOwner::Server)
            .await?;
        if owner != PublisherOwner::Server {
            return Err(DomainError::Store(format!(
                "the built-in publisher {BUILTIN_PUBLISHER:?} belongs to an account"
            )));
        }
        if tx.template(&template).await?.is_some() {
            tx.rollback().await?;
            return Ok(false);
        }
        let change_seq = tx.next_template_change().await?;
        let stored = StoredTemplate {
            template: template.clone(),
            title: definition.title().to_owned(),
            definition: definition.canonical_json(),
            status: TemplateStatus::Approved,
            submitted_by: None,
            submitted_at: now,
            reviewed_at: Some(now),
            change_seq: Some(change_seq),
        };
        if tx.insert_template(stored).await? == InsertOutcome::Duplicate {
            // Another seeder stored it meanwhile; give the change number back.
            tx.rollback().await?;
            return Ok(false);
        }
        tx.insert_template_event(TemplateEvent {
            template,
            action: TemplateAction::Seed,
            actor: None,
            reviewer: None,
            note: None,
            at: now,
        })
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Now, to the microsecond: what Postgres keeps, so every store returns the same time.
    fn template_now(&self) -> DateTime<Utc> {
        self.clock.now().trunc_subsecs(6)
    }
}

/// `limit`, with 0 and anything above [`MAX_CATALOG_PAGE`] read as [`MAX_CATALOG_PAGE`].
fn page_limit(limit: u32) -> u32 {
    if limit == 0 || limit > MAX_CATALOG_PAGE {
        MAX_CATALOG_PAGE
    } else {
        limit
    }
}

/// 1 to 64 characters, no control characters, not only whitespace.
fn validate_reviewer(reviewer: &str) -> Result<(), ValidationError> {
    let printable = !reviewer.chars().any(char::is_control);
    let length = reviewer.chars().count();
    if printable && length <= MAX_REVIEWER_CHARS && !reviewer.trim().is_empty() {
        Ok(())
    } else {
        Err(ValidationError::Reviewer)
    }
}

/// At most 500 characters, with line breaks the only control characters.
fn validate_note(note: &str) -> Result<(), ValidationError> {
    let printable = note.chars().all(|c| c == '\n' || !c.is_control());
    if printable && note.chars().count() <= MAX_NOTE_CHARS {
        Ok(())
    } else {
        Err(ValidationError::ReviewNote)
    }
}
