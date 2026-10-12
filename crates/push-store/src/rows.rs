//! Reading domain records from result rows.

use push_domain::{Conversation, ConversationId, InviteRecord, Member, UserId};
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
