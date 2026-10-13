//! Devices and sign-in tokens (spec §5.1, §5.3): registering a device with a registration
//! token, signing in again on a known device, refreshing with rotation and reuse
//! detection, listing and revoking devices, and checking access tokens.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::error::AuthError;
use crate::links::{Purpose, password_stamp};
use crate::outbox::{self, Event};
use crate::rules::{self, Platform};
use crate::service::{Auth, unix_now};
use crate::tokens::{
    ACCESS_TOKEN_TTL, REFRESH_TOKEN_TTL, TokenError, new_refresh_token, refresh_hash,
};

/// Most active devices per account (spec §5.3).
pub const MAX_ACTIVE_DEVICES: i64 = 10;

/// How long after its rotation the refresh token rotated last may be presented again
/// (spec §5.3), for a client whose refresh answer was lost.
pub const REFRESH_GRACE: Duration = Duration::from_secs(30);

/// The sign-in credentials of one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub access_token: String,
    pub access_expires_in_s: i64,
    pub refresh_token: String,
    pub refresh_expires_in_s: i64,
}

/// The signed-in user and device behind a valid access token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub user_id: Uuid,
    pub device_id: Uuid,
}

/// An active device, as `GET /v1/devices` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_id: Uuid,
    pub name: String,
    pub platform: Platform,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl Auth {
    /// A new refresh token for `device`, stored as its hash, and a new access token.
    async fn issue(
        &self,
        connection: &mut PgConnection,
        user_id: Uuid,
        device_id: Uuid,
    ) -> Result<Tokens, AuthError> {
        let refresh_token = new_refresh_token();
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + make_interval(secs => $3))",
        )
        .bind(refresh_hash(&refresh_token).as_slice())
        .bind(device_id)
        .bind(REFRESH_TOKEN_TTL.as_secs() as f64)
        .execute(&mut *connection)
        .await?;
        sqlx::query("UPDATE auth.device SET last_seen_at = now() WHERE device_id = $1")
            .bind(device_id)
            .execute(&mut *connection)
            .await?;
        let (access_token, _) = self.tokens.issue(user_id, device_id, unix_now());
        Ok(Tokens {
            user_id,
            device_id,
            access_token,
            access_expires_in_s: ACCESS_TOKEN_TTL.as_secs() as i64,
            refresh_token,
            refresh_expires_in_s: REFRESH_TOKEN_TTL.as_secs() as i64,
        })
    }

    /// `POST /v1/devices {name, platform, registration_id}` with a registration token:
    /// creates the device, writes `device.registered` and returns its tokens.
    ///
    /// `registration_id` is the client's idempotency key for this registration, like
    /// `client_msg_id` for a message. A retry with the same id for the same user finds
    /// the device the first attempt created and returns new tokens for it, revoking the
    /// earlier ones (their answer was lost), instead of creating a second device; it is
    /// not counted against the limit, and announces nothing. The id of a revoked device
    /// stays spent (`device_not_found`). A new id is refused when the account has
    /// [`MAX_ACTIVE_DEVICES`] active devices. The account row is locked, so concurrent
    /// registrations cannot pass the limit together and retries of one are serialised.
    ///
    /// The token binds the password hash of the sign-in that made it: after a password
    /// reset it registers nothing.
    pub async fn register_device(
        &self,
        registration_token: &str,
        registration_id: Uuid,
        name: &str,
        platform: &str,
    ) -> Result<Tokens, AuthError> {
        let claims = self
            .links
            .verify(registration_token, Purpose::Register, unix_now())
            .map_err(|_| AuthError::Unauthorized)?;
        let stamp = claims.pw.ok_or(AuthError::Unauthorized)?;
        let name = rules::device_name(name)?;
        let platform = Platform::parse(platform)?;
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT password_hash, email_verified_at IS NOT NULL FROM auth.user_account \
             WHERE user_id = $1 FOR UPDATE",
        )
        .bind(claims.sub)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account) = account else {
            return Err(AuthError::Unauthorized);
        };
        let password_hash: String = account.get(0);
        if password_stamp(&password_hash) != stamp {
            return Err(AuthError::Unauthorized);
        }
        if !account.get::<bool, _>(1) {
            return Err(AuthError::EmailNotVerified);
        }
        let earlier = sqlx::query(
            "SELECT device_id, revoked_at IS NULL FROM auth.device \
             WHERE user_id = $1 AND registration_id = $2",
        )
        .bind(claims.sub)
        .bind(registration_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(earlier) = earlier {
            let device_id: Uuid = earlier.get(0);
            if !earlier.get::<bool, _>(1) {
                return Err(AuthError::DeviceNotFound);
            }
            revoke_refresh_tokens(&mut tx, device_id).await?;
            let tokens = self.issue(&mut tx, claims.sub, device_id).await?;
            tx.commit().await?;
            return Ok(tokens);
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auth.device WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(claims.sub)
        .fetch_one(&mut *tx)
        .await?;
        if active >= MAX_ACTIVE_DEVICES {
            return Err(AuthError::TooManyDevices);
        }
        let device_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO auth.device (device_id, user_id, name, platform, registration_id) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(device_id)
        .bind(claims.sub)
        .bind(&name)
        .bind(platform.as_str())
        .bind(registration_id)
        .execute(&mut *tx)
        .await?;
        let tokens = self.issue(&mut tx, claims.sub, device_id).await?;
        let event = Event::DeviceRegistered {
            user_id: claims.sub,
            device_id,
        };
        outbox::write(&mut tx, &event).await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// `POST /v1/auth/login {email, password, device_id}` for an active device of the
    /// account: new tokens for it. Its earlier refresh tokens are revoked, since this
    /// sign-in replaces them.
    pub async fn login_device(
        &self,
        email: &str,
        password: &str,
        device_id: Uuid,
    ) -> Result<Tokens, AuthError> {
        let (user_id, _) = self.sign_in(email, password).await?;
        let mut tx = self.pool.begin().await?;
        let active = sqlx::query(
            "SELECT 1 FROM auth.device WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL FOR UPDATE",
        )
        .bind(device_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if active.is_none() {
            return Err(AuthError::DeviceNotFound);
        }
        revoke_refresh_tokens(&mut tx, device_id).await?;
        let tokens = self.issue(&mut tx, user_id, device_id).await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// `POST /v1/auth/refresh`: exchanges a refresh token for new tokens and retires it
    /// (rotation). Presenting a retired token again means it was copied: every refresh
    /// token of that device is revoked and the device must sign in again.
    ///
    /// One exception, for a client whose answer was lost: the token rotated last may be
    /// presented again within [`REFRESH_GRACE`] of its rotation. It gets a new pair, and
    /// the pair issued for it before is cut off — its refresh token counts as retired,
    /// with no grace window of its own — so only one branch lives. The window is not
    /// extended by such a retry. Presenting the token later, presenting an older one, or
    /// presenting a cut-off one is reuse as above. The presented token's row is locked
    /// first and its successor's second, so concurrent refreshes are serialised.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, AuthError> {
        let hash = refresh_hash(refresh_token);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT t.device_id, d.user_id, t.rotated_to, \
             t.revoked_at IS NULL AND t.expires_at > now() AND d.revoked_at IS NULL, \
             t.rotated_at > now() - make_interval(secs => $2) \
             FROM auth.refresh_token t JOIN auth.device d USING (device_id) \
             WHERE t.token_hash = $1 FOR UPDATE OF t",
        )
        .bind(hash.as_slice())
        .bind(REFRESH_GRACE.as_secs() as f64)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(AuthError::Unauthorized);
        };
        let device_id: Uuid = row.get(0);
        let user_id: Uuid = row.get(1);
        let rotated_to: Option<Vec<u8>> = row.get(2);
        if !row.get::<bool, _>(3) {
            return Err(AuthError::Unauthorized);
        }
        let Some(successor) = rotated_to else {
            let tokens = self.issue(&mut tx, user_id, device_id).await?;
            sqlx::query(
                "UPDATE auth.refresh_token SET rotated_to = $2, rotated_at = now() \
                 WHERE token_hash = $1",
            )
            .bind(hash.as_slice())
            .bind(refresh_hash(&tokens.refresh_token).as_slice())
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(tokens);
        };
        let in_grace = row.get::<Option<bool>, _>(4) == Some(true);
        let live_successor = in_grace
            && sqlx::query(
                "SELECT 1 FROM auth.refresh_token WHERE token_hash = $1 \
                 AND rotated_to IS NULL AND revoked_at IS NULL AND expires_at > now() \
                 FOR UPDATE",
            )
            .bind(&successor)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !live_successor {
            tracing::warn!(%device_id, "a retired refresh token was used again; revoking the device's tokens");
            revoke_refresh_tokens(&mut tx, device_id).await?;
            tx.commit().await?;
            return Err(AuthError::Unauthorized);
        }
        let tokens = self.issue(&mut tx, user_id, device_id).await?;
        let new_hash = refresh_hash(&tokens.refresh_token);
        sqlx::query(
            "UPDATE auth.refresh_token SET rotated_to = $2, rotated_at = NULL \
             WHERE token_hash = $1",
        )
        .bind(&successor)
        .bind(new_hash.as_slice())
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE auth.refresh_token SET rotated_to = $2 WHERE token_hash = $1")
            .bind(hash.as_slice())
            .bind(new_hash.as_slice())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// The caller behind an access token: valid, and its device still active.
    pub async fn authenticate(&self, access_token: &str) -> Result<Caller, AuthError> {
        let claims = self
            .tokens
            .verify(access_token)
            .map_err(|error| match error {
                TokenError::Expired => AuthError::TokenExpired,
                TokenError::Invalid => AuthError::Unauthorized,
            })?;
        let active = sqlx::query(
            "SELECT 1 FROM auth.device WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL",
        )
        .bind(claims.device_id)
        .bind(claims.sub)
        .fetch_optional(&self.pool)
        .await?;
        if active.is_none() {
            return Err(AuthError::Unauthorized);
        }
        Ok(Caller {
            user_id: claims.sub,
            device_id: claims.device_id,
        })
    }

    /// `GET /v1/devices`: the caller's active devices, oldest first.
    pub async fn list_devices(&self, caller: Caller) -> Result<Vec<DeviceInfo>, AuthError> {
        let rows = sqlx::query(
            "SELECT device_id, name, platform, created_at, last_seen_at FROM auth.device \
             WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at, device_id",
        )
        .bind(caller.user_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(DeviceInfo {
                    device_id: row.get(0),
                    name: row.get(1),
                    platform: Platform::parse(row.get(2)).map_err(|_| AuthError::Unavailable)?,
                    created_at: row.get(3),
                    last_seen_at: row.get(4),
                })
            })
            .collect()
    }

    /// `DELETE /v1/devices/{id}`: revokes one of the caller's devices (the caller's own
    /// included) and its refresh tokens, and writes `device.revoked`, on which
    /// message-svc kicks it (spec §5.3).
    pub async fn revoke_device(&self, caller: Caller, device_id: Uuid) -> Result<(), AuthError> {
        let mut tx = self.pool.begin().await?;
        let revoked = sqlx::query(
            "UPDATE auth.device SET revoked_at = now() WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL RETURNING device_id",
        )
        .bind(device_id)
        .bind(caller.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if revoked.is_none() {
            return Err(AuthError::DeviceNotFound);
        }
        revoke_refresh_tokens(&mut tx, device_id).await?;
        let event = Event::DeviceRevoked {
            user_id: caller.user_id,
            device_id,
        };
        outbox::write(&mut tx, &event).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn revoke_refresh_tokens(
    connection: &mut PgConnection,
    device_id: Uuid,
) -> Result<(), AuthError> {
    sqlx::query(
        "UPDATE auth.refresh_token SET revoked_at = now() \
         WHERE device_id = $1 AND revoked_at IS NULL",
    )
    .bind(device_id)
    .execute(connection)
    .await?;
    Ok(())
}
