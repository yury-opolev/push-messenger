//! RICH and ANSWER enforcement (spec 2026-10-26, rich message templates, D1, D9–D11 and
//! "Enforcement"). The body checks run before the append transaction; the checks that
//! read the registry and the card run inside it, after a retry has been answered.

use crate::error::{DomainError, DomainResult};
use crate::ids::{MessageId, UserId};
use crate::model::{ContentType, Entry, EntryKind, Member, TemplateStatus};
use crate::store::Tx;
use crate::templates::{AnswerBody, Definition, RichBody, SenderKind, TemplateError, TemplateRef};
use crate::validation::{ValidationError, validate_text};

/// A message body, read as its content type.
pub(crate) enum Content {
    Text,
    Rich(RichBody),
    Answer(AnswerBody),
}

impl Content {
    /// Reads `body` as `content_type`, without I/O. A system event cannot be sent.
    pub(crate) fn parse(content_type: ContentType, body: &[u8]) -> DomainResult<Content> {
        match content_type {
            ContentType::Text => {
                validate_text(body)?;
                Ok(Content::Text)
            }
            ContentType::Rich => Ok(Content::Rich(RichBody::parse(body)?)),
            ContentType::Answer => Ok(Content::Answer(AnswerBody::parse(body)?)),
            ContentType::SystemEvent => Err(ValidationError::ContentType.into()),
        }
    }
}

/// The definition of `template` if it is stored and approved. Missing, pending, rejected
/// and revoked versions get the same refusal, so a send is no oracle for the registry.
pub(crate) async fn approved_definition<T: Tx>(
    tx: &mut T,
    template: &TemplateRef,
) -> DomainResult<Definition> {
    let stored = tx
        .template(template)
        .await?
        .filter(|stored| stored.status == TemplateStatus::Approved)
        .ok_or_else(|| TemplateError(format!("template {template} is not approved")))?;
    // It passed every rule when it was submitted; failing now is a storage fault.
    Definition::parse(stored.definition.as_bytes()).map_err(|error| {
        DomainError::Store(format!(
            "the stored definition of {template} is invalid: {error}"
        ))
    })
}

/// Checks a RICH body sent, or edited in, by `sender`: its template is approved, its
/// params match the definition, and the definition allows the sender's kind (spec D9:
/// `agent` for an owned agent, otherwise `human`).
pub(crate) async fn check_rich<T: Tx>(
    tx: &mut T,
    sender: UserId,
    body: &RichBody,
) -> DomainResult<()> {
    let definition = approved_definition(tx, &body.template).await?;
    body.check(&definition)?;
    let kind = if tx.is_agent(sender).await? {
        SenderKind::Agent
    } else {
        SenderKind::Human
    };
    if !definition.allows(kind) {
        return Err(TemplateError(format!(
            "template {} cannot be sent by a {kind}",
            body.template
        ))
        .into());
    }
    Ok(())
}

/// Checks an ANSWER by `answerer` and returns the card it answers (spec D10): a RICH
/// message of the conversation that the answerer may see, not deleted, sent by someone
/// else and not answered yet, whose template is still approved and offers the values.
/// Any sender kind may answer.
pub(crate) async fn check_answer<T: Tx>(
    tx: &mut T,
    answerer: &Member,
    body: &AnswerBody,
) -> DomainResult<Entry> {
    let card = tx
        .entry_by_message_id(answerer.conversation_id, MessageId(body.card))
        .await?
        .filter(|entry| {
            entry.kind == EntryKind::Message
                && entry.seq >= answerer.visible_from_seq
                && entry.deleted_at.is_none()
        })
        .ok_or(DomainError::TargetNotFound)?;
    if card.content_type != ContentType::Rich {
        return Err(TemplateError("the target is not a card".into()).into());
    }
    if card.sender_id == answerer.user_id {
        return Err(TemplateError("you cannot answer your own card".into()).into());
    }
    if card.answer_seq.is_some() {
        return Err(DomainError::CardAnswered);
    }
    let card_body = card
        .body
        .as_deref()
        .ok_or_else(|| DomainError::Store("a card without a body".into()))
        .and_then(|bytes| {
            RichBody::parse(bytes)
                .map_err(|error| DomainError::Store(format!("a stored card is invalid: {error}")))
        })?;
    let definition = approved_definition(tx, &card_body.template).await?;
    body.check(&card_body, &definition)?;
    Ok(card)
}
