//! Reading domain records from result rows.

use push_domain::{InviteRecord, UserId};
use sqlx::Row;
use sqlx::postgres::PgRow;

fn decode_error(message: String) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
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
