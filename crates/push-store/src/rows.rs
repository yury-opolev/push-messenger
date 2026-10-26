//! Reading domain records from result rows.

use push_domain::templates::TemplateRef;
use push_domain::{
    ClientMsgId, Conversation, ConversationId, DeviceId, Entry, InviteRecord, Member, MessageId,
    StoredTemplate, TemplateAction, TemplateEvent, UserId,
};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::codes::decode;

/// The columns [`conversation`] reads, from a table aliased `c`.
macro_rules! conversation_columns {
    () => {
        "c.conversation_id, c.kind, c.direct_key, c.title, c.created_by, c.last_seq, \
         c.created_at, c.updated_at"
    };
}
pub(crate) use conversation_columns;

/// The columns [`member`] reads, from a table aliased `m`.
macro_rules! member_columns {
    () => {
        "m.conversation_id, m.user_id, m.role, m.visible_from_seq, m.left_at_seq, m.read_seq"
    };
}
pub(crate) use member_columns;

/// The columns [`entry`] reads, from a table aliased `e`.
macro_rules! entry_columns {
    () => {
        "e.conversation_id, e.seq, e.message_id, e.client_msg_id, e.kind, e.target_seq, \
         e.sender_id, e.sender_device_id, e.content_type, e.body, e.client_ts, e.server_ts, \
         e.edited_at, e.edit_seq, e.deleted_at"
    };
}
pub(crate) use entry_columns;

/// The columns [`template`] reads, from a table aliased `t`.
macro_rules! template_columns {
    () => {
        "t.publisher, t.name, t.version, t.title, t.definition, t.status, t.submitted_by, \
         t.submitted_at, t.reviewed_at, t.change_seq"
    };
}
pub(crate) use template_columns;

fn decode_error(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

pub(crate) fn conversation(row: &PgRow) -> Result<Conversation, sqlx::Error> {
    let direct_key: Option<Vec<u8>> = row.try_get("direct_key")?;
    let direct_key = direct_key
        .map(|key| {
            <[u8; 32]>::try_from(key)
                .map_err(|key| decode_error(format!("direct_key has {} bytes", key.len())))
        })
        .transpose()?;
    Ok(Conversation {
        id: ConversationId(row.try_get("conversation_id")?),
        kind: decode(row.try_get("kind")?)?,
        direct_key,
        title: row.try_get("title")?,
        created_by: UserId(row.try_get("created_by")?),
        last_seq: row.try_get("last_seq")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

pub(crate) fn member(row: &PgRow) -> Result<Member, sqlx::Error> {
    Ok(Member {
        conversation_id: ConversationId(row.try_get("conversation_id")?),
        user_id: UserId(row.try_get("user_id")?),
        role: decode(row.try_get("role")?)?,
        visible_from_seq: row.try_get("visible_from_seq")?,
        left_at_seq: row.try_get("left_at_seq")?,
        read_seq: row.try_get("read_seq")?,
    })
}

pub(crate) fn entry(row: &PgRow) -> Result<Entry, sqlx::Error> {
    let sender_device_id: Option<uuid::Uuid> = row.try_get("sender_device_id")?;
    Ok(Entry {
        conversation_id: ConversationId(row.try_get("conversation_id")?),
        seq: row.try_get("seq")?,
        message_id: MessageId(row.try_get("message_id")?),
        client_msg_id: ClientMsgId(row.try_get("client_msg_id")?),
        kind: decode(row.try_get("kind")?)?,
        target_seq: row.try_get("target_seq")?,
        sender_id: UserId(row.try_get("sender_id")?),
        sender_device_id: sender_device_id.map(DeviceId),
        content_type: decode(row.try_get("content_type")?)?,
        body: row.try_get("body")?,
        client_ts: row.try_get("client_ts")?,
        server_ts: row.try_get("server_ts")?,
        edited_at: row.try_get("edited_at")?,
        edit_seq: row.try_get("edit_seq")?,
        deleted_at: row.try_get("deleted_at")?,
        // Not stored on Postgres until migration 0005 adds `message.answer_seq`.
        answer_seq: None,
    })
}

pub(crate) fn invite(row: &PgRow) -> Result<InviteRecord, sqlx::Error> {
    let count = |column: &str| -> Result<u32, sqlx::Error> {
        let value: i32 = row.try_get(column)?;
        u32::try_from(value).map_err(|_| decode_error(format!("invite.{column} is {value}")))
    };
    Ok(InviteRecord {
        code: row.try_get("code")?,
        user_id: UserId(row.try_get("user_id")?),
        expires_at: row.try_get("expires_at")?,
        max_uses: count("max_uses")?,
        uses: count("uses")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

/// The version a row names, from its `publisher`, `name` and `version` columns.
fn template_ref(row: &PgRow) -> Result<TemplateRef, sqlx::Error> {
    let version: i32 = row.try_get("version")?;
    Ok(TemplateRef {
        publisher: row.try_get("publisher")?,
        name: row.try_get("name")?,
        version: u16::try_from(version)
            .map_err(|_| decode_error(format!("template version is {version}")))?,
    })
}

pub(crate) fn template(row: &PgRow) -> Result<StoredTemplate, sqlx::Error> {
    let submitted_by: Option<uuid::Uuid> = row.try_get("submitted_by")?;
    Ok(StoredTemplate {
        template: template_ref(row)?,
        title: row.try_get("title")?,
        definition: row.try_get("definition")?,
        status: decode(row.try_get("status")?)?,
        submitted_by: submitted_by.map(UserId),
        submitted_at: row.try_get("submitted_at")?,
        reviewed_at: row.try_get("reviewed_at")?,
        change_seq: row.try_get("change_seq")?,
    })
}

pub(crate) fn template_event(row: &PgRow) -> Result<TemplateEvent, sqlx::Error> {
    let action: String = row.try_get("action")?;
    let actor: Option<uuid::Uuid> = row.try_get("actor")?;
    Ok(TemplateEvent {
        template: template_ref(row)?,
        action: TemplateAction::ALL
            .into_iter()
            .find(|known| known.as_str() == action)
            .ok_or_else(|| decode_error(format!("unknown template_event.action {action:?}")))?,
        actor: actor.map(UserId),
        reviewer: row.try_get("reviewer")?,
        note: row.try_get("note")?,
        at: row.try_get("at")?,
    })
}
