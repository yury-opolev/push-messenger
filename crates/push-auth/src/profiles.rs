//! Profiles and handles (spec §5.5): the caller's own profile, and finding a user by
//! @handle. Emails are never shown to anyone but their owner.

use sqlx::Row;
use uuid::Uuid;

use crate::devices::Caller;
use crate::error::AuthError;
use crate::rules;
use crate::service::Auth;

/// What anyone may see of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicProfile {
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
}

/// The caller's own profile, with their email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnProfile {
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
    pub email: String,
}

impl Auth {
    /// `GET /v1/users/me`.
    pub async fn own_profile(&self, caller: Caller) -> Result<OwnProfile, AuthError> {
        let row = sqlx::query(
            "SELECT handle::text, display_name, email::text FROM auth.user_account \
             WHERE user_id = $1",
        )
        .bind(caller.user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::Unauthorized)?;
        Ok(OwnProfile {
            user_id: caller.user_id,
            handle: row.get(0),
            display_name: row.get(1),
            email: row.get(2),
        })
    }

    /// `PATCH /v1/users/me {display_name}`.
    pub async fn set_display_name(
        &self,
        caller: Caller,
        display_name: &str,
    ) -> Result<OwnProfile, AuthError> {
        let display_name = rules::display_name(display_name)?;
        sqlx::query("UPDATE auth.user_account SET display_name = $2 WHERE user_id = $1")
            .bind(caller.user_id)
            .bind(&display_name)
            .execute(&self.pool)
            .await?;
        self.own_profile(caller).await
    }

    /// `GET /v1/users/lookup?handle=…`: the verified user with this handle (with or
    /// without the `@`, in any case). Unverified accounts are not users yet.
    pub async fn lookup(&self, handle: &str) -> Result<PublicProfile, AuthError> {
        let handle = rules::handle(handle)?;
        let row = sqlx::query(
            "SELECT user_id, handle::text, display_name FROM auth.user_account \
             WHERE handle = $1::citext AND email_verified_at IS NOT NULL",
        )
        .bind(&handle)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::UserNotFound)?;
        Ok(PublicProfile {
            user_id: row.get(0),
            handle: row.get(1),
            display_name: row.get(2),
        })
    }
}
