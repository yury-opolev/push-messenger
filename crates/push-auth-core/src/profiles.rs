//! Profiles and handles (spec §5.5): the caller's own profile, finding a user by @handle,
//! and the profiles of users the caller shares a contact or a conversation with. Emails
//! are never shown to anyone but their owner.

use sqlx::Row;
use uuid::Uuid;

use crate::devices::Caller;
use crate::error::AuthError;
use crate::rules::{self, Invalid};
use crate::service::{Auth, is_violation};

/// What anyone may see of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicProfile {
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
    pub kind: String,
    pub owner_user_id: Option<Uuid>,
}

/// The caller's own profile, with their email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnProfile {
    pub user_id: Uuid,
    pub handle: String,
    pub display_name: String,
    /// The account's address; for an Entra ID account the latest address its token
    /// carried (informational), if any.
    pub email: Option<String>,
    pub kind: String,
    pub owner_user_id: Option<Uuid>,
}

impl Auth {
    /// `GET /v1/users/me`.
    pub async fn own_profile(&self, caller: Caller) -> Result<OwnProfile, AuthError> {
        let row = sqlx::query(
                    "SELECT account.handle::text, account.display_name, \
                     coalesce(account.email::text, identity.email), account.kind, account.owner_user_id \
                     FROM auth.user_account AS account \
                     LEFT JOIN auth.external_identity AS identity ON identity.user_id = account.user_id \
                     WHERE account.user_id = $1 AND account.disabled_at IS NULL AND account.email_verified_at IS NOT NULL",
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
            kind: row.get(3),
            owner_user_id: row.get(4),
        })
    }

    /// `PATCH /v1/users/me {display_name?, handle?}` (spec D14): changes the display name,
    /// the handle or both, at least one, in one transaction. A handle any other account
    /// holds is `handle_taken` (the unique constraint decides); the old handle is free at
    /// once. Nothing else stores handles, so nothing else changes.
    pub async fn update_profile(
        &self,
        caller: Caller,
        display_name: Option<&str>,
        handle: Option<&str>,
    ) -> Result<OwnProfile, AuthError> {
        let display_name = display_name.map(rules::display_name).transpose()?;
        let handle = handle.map(rules::handle).transpose()?;
        if display_name.is_none() && handle.is_none() {
            return Err(AuthError::Invalid(Invalid {
                field: "body",
                reason: "give display_name, handle or both",
            }));
        }
        crate::agents::expire(&self.pool).await?;
        let mut tx = self.pool.begin().await?;
        self.lock_enabled_account(&mut tx, caller.user_id).await?;
        if let Some(display_name) = &display_name {
            sqlx::query("UPDATE auth.user_account SET display_name = $2 WHERE user_id = $1")
                .bind(caller.user_id)
                .bind(display_name)
                .execute(&mut *tx)
                .await?;
        }
        let mut renamed = false;
        if let Some(handle) = &handle {
            let changed = sqlx::query(
                "UPDATE auth.user_account SET handle = $2 \
                 WHERE user_id = $1 AND handle <> $2::citext RETURNING 1",
            )
            .bind(caller.user_id)
            .bind(handle)
            .fetch_optional(&mut *tx)
            .await;
            match changed {
                Ok(row) => renamed = row.is_some(),
                Err(error) if is_violation(&error, "user_account_handle_key") => {
                    return Err(AuthError::HandleTaken);
                }
                Err(error) => return Err(error.into()),
            }
        }
        tx.commit().await?;
        if renamed {
            tracing::info!(
                target: "security",
                event = "auth.handle.changed",
                user_id = %caller.user_id,
                "a user changed their handle"
            );
        }
        self.own_profile(caller).await
    }

    /// `PATCH /v1/users/me {display_name}`.
    pub async fn set_display_name(
        &self,
        caller: Caller,
        display_name: &str,
    ) -> Result<OwnProfile, AuthError> {
        self.update_profile(caller, Some(display_name), None).await
    }

    /// `GET /v1/users/lookup?handle=…`: the verified user with this handle (with or
    /// without the `@`, in any case). Unverified accounts are not users yet.
    pub async fn lookup(&self, handle: &str) -> Result<PublicProfile, AuthError> {
        let handle = rules::handle(handle)?;
        let row = sqlx::query(
            "SELECT user_id, handle::text, display_name, kind, owner_user_id FROM auth.user_account \
             WHERE handle = $1::citext AND email_verified_at IS NOT NULL AND disabled_at IS NULL",
        )
        .bind(&handle)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::UserNotFound)?;
        Ok(PublicProfile {
            user_id: row.get(0),
            handle: row.get(1),
            display_name: row.get(2),
            kind: row.get(3),
            owner_user_id: row.get(4),
        })
    }

    /// The profiles of the verified users among `users`, sorted by user id. The caller
    /// checks first that they may be shown (`GET /v1/users?ids=…` asks message-svc).
    pub async fn profiles(&self, users: &[Uuid]) -> Result<Vec<PublicProfile>, AuthError> {
        let rows = sqlx::query(
            "SELECT user_id, handle::text, display_name, kind, owner_user_id FROM auth.user_account \
             WHERE user_id = ANY($1) AND email_verified_at IS NOT NULL AND disabled_at IS NULL ORDER BY user_id",
        )
        .bind(users)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| PublicProfile {
                user_id: row.get(0),
                handle: row.get(1),
                display_name: row.get(2),
                kind: row.get(3),
                owner_user_id: row.get(4),
            })
            .collect())
    }
}
