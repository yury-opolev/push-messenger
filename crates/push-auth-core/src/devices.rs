//! Devices and sign-in tokens (spec §5.1, §5.3): registering a device with a registration
//! token, signing in again on a known device, refreshing with rotation and reuse
//! detection, listing and revoking devices, and checking access tokens.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::AuthError;
use crate::links::{LinkClaims, Purpose, password_stamp};
use crate::outbox::{self, Event};
use crate::rules::{self, Platform};
use crate::service::{Auth, unix_now};
use crate::tokens::{
    ACCESS_TOKEN_TTL, REFRESH_TOKEN_TTL, TokenError, new_refresh_token, refresh_hash,
};

/// Devices revoked per transaction by the Entra ID-only sweep.
const SWEEP_BATCH: i64 = 500;
/// How soon the sweep looks again at password devices another transaction held.
const SWEEP_RETRY: Duration = Duration::from_millis(250);

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
    ///
    /// Every change to a device's tokens happens under the lock of its device row, taken
    /// first (after the account row, where that is locked): the caller holds it, so the
    /// `last_seen_at` update below takes no second lock, and a revocation, which sees
    /// the tokens as of after it got the lock, cannot miss a token inserted here.
    pub(crate) async fn issue(
        &self,
        connection: &mut PgConnection,
        user_id: Uuid,
        device_id: Uuid,
        session_expires_at: Option<i64>,
    ) -> Result<Tokens, AuthError> {
        let refresh_token = new_refresh_token();
        let full = REFRESH_TOKEN_TTL.as_secs() as i64;
        // A refresh token never outlives the device's session (spec D8).
        let refresh_seconds =
            session_expires_at.map_or(full, |end| (end - unix_now()).clamp(0, full));
        sqlx::query(
            "INSERT INTO auth.refresh_token (token_hash, device_id, expires_at) \
             VALUES ($1, $2, now() + make_interval(secs => $3))",
        )
        .bind(refresh_hash(&refresh_token).as_slice())
        .bind(device_id)
        .bind(refresh_seconds as f64)
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
            refresh_expires_in_s: refresh_seconds,
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
    /// The token binds the password hash of the sign-in that made it (after a password
    /// reset it registers nothing), or the Entra ID identity that signed in, whose session
    /// end the device takes (spec D7, D8).
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
        let session = self
            .registration_binding(&mut tx, &claims, account.get(0))
            .await?;
        if !account.get::<bool, _>(1) {
            return Err(AuthError::EmailNotVerified);
        }
        let earlier = sqlx::query(
            "SELECT device_id, revoked_at IS NULL FROM auth.device \
             WHERE user_id = $1 AND registration_id = $2 FOR UPDATE",
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
            set_session(&mut tx, device_id, session).await?;
            revoke_refresh_tokens(&mut tx, device_id).await?;
            let tokens = self.issue(&mut tx, claims.sub, device_id, session).await?;
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
            "INSERT INTO auth.device \
             (device_id, user_id, name, platform, registration_id, session_expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(device_id)
        .bind(claims.sub)
        .bind(&name)
        .bind(platform.as_str())
        .bind(registration_id)
        .bind(session)
        .execute(&mut *tx)
        .await?;
        let tokens = self.issue(&mut tx, claims.sub, device_id, session).await?;
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
        let tokens = self.issue(&mut tx, user_id, device_id, None).await?;
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
    /// presenting a cut-off one is reuse as above. The device row is locked first, then the
    /// presented token's row and its successor's, so every change to a device's tokens is
    /// serialised, and a revocation cannot miss a token a concurrent refresh issues.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, AuthError> {
        let hash = refresh_hash(refresh_token);
        let mut tx = self.pool.begin().await?;
        let device_id: Option<Uuid> =
            sqlx::query_scalar("SELECT device_id FROM auth.refresh_token WHERE token_hash = $1")
                .bind(hash.as_slice())
                .fetch_optional(&mut *tx)
                .await?;
        let Some(device_id) = device_id else {
            return Err(AuthError::Unauthorized);
        };
        let device = sqlx::query(
            "SELECT device.user_id, device.revoked_at IS NULL, device.session_expires_at, \
             account.password_hash IS NOT NULL \
             FROM auth.device AS device JOIN auth.user_account AS account USING (user_id) \
             WHERE device.device_id = $1 FOR UPDATE OF device",
        )
        .bind(device_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(device) = device else {
            return Err(AuthError::Unauthorized);
        };
        let user_id: Uuid = device.get(0);
        if !device.get::<bool, _>(1) {
            return Err(AuthError::Unauthorized);
        }
        // An Entra-backed device past its maximum age signs in with Entra ID again; the
        // device and its tokens stay as they are (spec D8).
        let session: Option<i64> = device.get(2);
        if session.is_some_and(|end| end <= unix_now()) {
            return Err(AuthError::SessionExpired);
        }
        // Entra ID only: password sessions ended with the switch (spec D16). The sweep
        // may not have reached this device (it skips rows that are locked, as this one is
        // now), so it is revoked here, with `device.revoked` to close its connections.
        if device.get::<bool, _>(3) && !self.password_enabled() {
            revoke_devices(&mut tx, &[(user_id, device_id)]).await?;
            tx.commit().await?;
            return Err(AuthError::Unauthorized);
        }
        // Read again under the device lock: what the first look saw may have changed. The
        // clock is the statement's, not the transaction's: the lock wait must not stretch
        // the grace window.
        let row = sqlx::query(
            "SELECT rotated_to, revoked_at IS NULL AND expires_at > statement_timestamp(), \
             rotated_at > statement_timestamp() - make_interval(secs => $3) \
             FROM auth.refresh_token WHERE token_hash = $1 AND device_id = $2 FOR UPDATE",
        )
        .bind(hash.as_slice())
        .bind(device_id)
        .bind(REFRESH_GRACE.as_secs() as f64)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Err(AuthError::Unauthorized);
        };
        let rotated_to: Option<Vec<u8>> = row.get(0);
        if !row.get::<bool, _>(1) {
            return Err(AuthError::Unauthorized);
        }
        let Some(successor) = rotated_to else {
            let tokens = self.issue(&mut tx, user_id, device_id, session).await?;
            sqlx::query(
                "UPDATE auth.refresh_token SET rotated_to = $2, rotated_at = statement_timestamp() \
                 WHERE token_hash = $1",
            )
            .bind(hash.as_slice())
            .bind(refresh_hash(&tokens.refresh_token).as_slice())
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(tokens);
        };
        let in_grace = row.get::<Option<bool>, _>(2) == Some(true);
        let live_successor = in_grace
            && sqlx::query(
                "SELECT 1 FROM auth.refresh_token WHERE token_hash = $1 \
                 AND rotated_to IS NULL AND revoked_at IS NULL AND expires_at > statement_timestamp() \
                 FOR UPDATE",
            )
            .bind(&successor)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !live_successor {
            tracing::warn!(
                target: "security",
                event = "auth.refresh.reuse",
                %user_id,
                %device_id,
                "a retired refresh token was used again; revoking the device's tokens"
            );
            revoke_refresh_tokens(&mut tx, device_id).await?;
            tx.commit().await?;
            return Err(AuthError::Unauthorized);
        }
        let tokens = self.issue(&mut tx, user_id, device_id, session).await?;
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

    /// The caller behind an access token: valid, its device still active, and issued in a
    /// later second than the account's last password reset (the cutoff message-svc applies
    /// too: within that second the order is unknown, and refusing a token minted just
    /// after the reset only asks for one more sign-in).
    pub async fn authenticate(&self, access_token: &str) -> Result<Caller, AuthError> {
        let claims = self
            .tokens
            .verify(access_token)
            .map_err(|error| match error {
                TokenError::Expired => AuthError::TokenExpired,
                TokenError::Invalid => AuthError::Unauthorized,
            })?;
        let account: Option<(Option<i64>, bool)> = sqlx::query_as(
            "SELECT account.password_reset_at, account.password_hash IS NOT NULL \
             FROM auth.device AS device \
             JOIN auth.user_account AS account ON account.user_id = device.user_id \
             WHERE device.device_id = $1 AND device.user_id = $2 \
             AND device.revoked_at IS NULL",
        )
        .bind(claims.device_id)
        .bind(claims.sub)
        .fetch_optional(&self.pool)
        .await?;
        match account {
            None => return Err(AuthError::Unauthorized),
            Some((Some(reset_at), _)) if claims.iat <= reset_at => {
                return Err(AuthError::Unauthorized);
            }
            // Entra ID only: a password device the sweep has not reached yet is refused
            // all the same (spec D16).
            Some((_, true)) if !self.password_enabled() => return Err(AuthError::Unauthorized),
            Some(_) => {}
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
        // The update locks the device row before the tokens are touched.
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

    /// What a registration token is bound to (spec D7): the password hash of its sign-in
    /// (`None`: no session end), or the Entra ID identity that signed in (`Some`: the
    /// session end it carries). A token bound to neither or to both, to a changed password,
    /// to another account's identity, or to a way of signing in the service no longer
    /// offers registers nothing.
    async fn registration_binding(
        &self,
        connection: &mut PgConnection,
        claims: &LinkClaims,
        password_hash: Option<String>,
    ) -> Result<Option<i64>, AuthError> {
        match (&claims.pw, claims.xid, claims.sx) {
            (Some(stamp), None, None) if self.password_enabled() => {
                if password_hash.is_some_and(|hash| password_stamp(&hash) == *stamp) {
                    Ok(None)
                } else {
                    Err(AuthError::Unauthorized)
                }
            }
            (None, Some(identity), Some(session)) if self.entra().is_some() => {
                let bound = sqlx::query(
                    "SELECT 1 FROM auth.external_identity WHERE identity_id = $1 AND user_id = $2",
                )
                .bind(identity)
                .bind(claims.sub)
                .fetch_optional(&mut *connection)
                .await?;
                if bound.is_some() {
                    Ok(Some(session))
                } else {
                    Err(AuthError::Unauthorized)
                }
            }
            _ => Err(AuthError::Unauthorized),
        }
    }

    /// Signs `user_id` out of every device (spec D15): each active device is revoked with
    /// its refresh tokens, and `device.revoked` closes its connections through the usual
    /// relay. Returns how many devices were active. The account stays.
    pub async fn revoke_user_sessions(&self, user_id: Uuid) -> Result<u64, AuthError> {
        let mut tx = self.pool.begin().await?;
        // The account row first, as registration locks it: a device registered meanwhile
        // is either seen here or registered after this sign-out, never missed.
        sqlx::query("SELECT 1 FROM auth.user_account WHERE user_id = $1 FOR UPDATE")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        // Device rows next, in a fixed order, like every other change to a device's tokens.
        let devices: Vec<Uuid> = sqlx::query_scalar(
            "SELECT device_id FROM auth.device WHERE user_id = $1 AND revoked_at IS NULL \
             ORDER BY device_id FOR UPDATE",
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
        let pairs: Vec<(Uuid, Uuid)> = devices.iter().map(|device| (user_id, *device)).collect();
        revoke_devices(&mut tx, &pairs).await?;
        tx.commit().await?;
        Ok(pairs.len() as u64)
    }

    /// Entra ID only (spec D16): revokes, in batches, the active devices of accounts that
    /// sign in with a password, as an admin revocation does, until none is left. Rows
    /// another transaction holds (another replica's sweep, a refresh, an old replica's
    /// sign-in during a rolling update) are skipped and tried again shortly after, until a
    /// look that skips nothing finds no such device; so several replicas share the work,
    /// each device is revoked once, and none is missed. Returns how many this call revoked
    /// (0 once nothing is left); when `shutdown` is cancelled it stops and returns what it
    /// revoked so far.
    pub async fn revoke_password_devices(
        &self,
        shutdown: &CancellationToken,
    ) -> Result<u64, AuthError> {
        let mut total = self.revoke_unlocked_password_devices().await?;
        loop {
            let left: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM auth.device AS device \
                 JOIN auth.user_account AS account USING (user_id) \
                 WHERE device.revoked_at IS NULL AND account.password_hash IS NOT NULL)",
            )
            .fetch_one(&self.pool)
            .await?;
            if !left {
                return Ok(total);
            }
            tokio::select! {
                () = shutdown.cancelled() => return Ok(total),
                () = tokio::time::sleep(SWEEP_RETRY) => {}
            }
            total += self.revoke_unlocked_password_devices().await?;
        }
    }

    /// One pass of [`Auth::revoke_password_devices`] over the rows nobody holds.
    async fn revoke_unlocked_password_devices(&self) -> Result<u64, AuthError> {
        let mut total = 0;
        loop {
            let mut tx = self.pool.begin().await?;
            let rows = sqlx::query(
                "SELECT device.user_id, device.device_id FROM auth.device AS device \
                 JOIN auth.user_account AS account USING (user_id) \
                 WHERE device.revoked_at IS NULL AND account.password_hash IS NOT NULL \
                 ORDER BY device.device_id LIMIT $1 FOR UPDATE OF device SKIP LOCKED",
            )
            .bind(SWEEP_BATCH)
            .fetch_all(&mut *tx)
            .await?;
            if rows.is_empty() {
                tx.commit().await?;
                return Ok(total);
            }
            let devices: Vec<(Uuid, Uuid)> =
                rows.iter().map(|row| (row.get(0), row.get(1))).collect();
            revoke_devices(&mut tx, &devices).await?;
            tx.commit().await?;
            total += devices.len() as u64;
        }
    }
}

pub(crate) async fn revoke_refresh_tokens(
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

/// Revokes `devices` (`(user, device)` pairs whose rows the caller has locked) and their
/// refresh tokens, and writes `device.revoked` for each: message-svc then refuses their
/// access tokens and the gateway closes their connections (4003).
pub(crate) async fn revoke_devices(
    connection: &mut PgConnection,
    devices: &[(Uuid, Uuid)],
) -> Result<(), AuthError> {
    if devices.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = devices.iter().map(|(_, device)| *device).collect();
    sqlx::query(
        "UPDATE auth.device SET revoked_at = now() \
         WHERE device_id = ANY($1) AND revoked_at IS NULL",
    )
    .bind(&ids)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE auth.refresh_token SET revoked_at = now() \
         WHERE device_id = ANY($1) AND revoked_at IS NULL",
    )
    .bind(&ids)
    .execute(&mut *connection)
    .await?;
    for (user_id, device_id) in devices {
        let event = Event::DeviceRevoked {
            user_id: *user_id,
            device_id: *device_id,
        };
        outbox::write(&mut *connection, &event).await?;
    }
    Ok(())
}

/// Starts a new session for an Entra-backed device; nothing for a password device.
pub(crate) async fn set_session(
    connection: &mut PgConnection,
    device_id: Uuid,
    session: Option<i64>,
) -> Result<(), AuthError> {
    if let Some(session) = session {
        sqlx::query("UPDATE auth.device SET session_expires_at = $2 WHERE device_id = $1")
            .bind(device_id)
            .bind(session)
            .execute(connection)
            .await?;
    }
    Ok(())
}
