//! Entra ID accounts (spec 2026-10-19 D5 to D8): found by `(tenant, object id)`, created
//! on the first sign-in with `user.verified` in the same transaction, never linked to an
//! email/password account; registration tokens bound to the identity; device sign-ins that
//! renew the session.

use rand::RngCore;
use sqlx::Row;
use uuid::Uuid;

use crate::devices::{Tokens, revoke_refresh_tokens, set_session};
use crate::entra::EntraIdentity;
use crate::error::AuthError;
use crate::links::{LinkClaims, Purpose};
use crate::outbox::{self, Event};
use crate::rules;
use crate::service::{Auth, REGISTRATION_TOKEN_SECONDS, RegistrationToken, unix_now};

/// What an Entra ID sign-in without a device gives: the account and a registration token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraRegistration {
    pub user_id: Uuid,
    pub registration: RegistrationToken,
}

/// An Entra ID account: the user and the identity row that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EntraAccount {
    user_id: Uuid,
    identity_id: Uuid,
}

/// Attempts at a free generated handle.
const HANDLE_ATTEMPTS: usize = 5;

impl Auth {
    /// `POST /v1/auth/entra {token}`: the account of `identity`, created on its first
    /// sign-in, and a registration token bound to the identity and to the end of the
    /// session it starts. `handle` names a new account (it is ignored afterwards); without
    /// it a random `u…` handle is chosen.
    pub async fn sign_in_entra(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
    ) -> Result<EntraRegistration, AuthError> {
        let max_session = self.max_session()?;
        let account = self.entra_account(identity, handle).await?;
        let now = unix_now();
        let token = self.links.sign(&LinkClaims {
            sub: account.user_id,
            purpose: Purpose::Register,
            exp: now + REGISTRATION_TOKEN_SECONDS,
            email: None,
            pw: None,
            xid: Some(account.identity_id),
            sx: Some(now + max_session),
        });
        Ok(EntraRegistration {
            user_id: account.user_id,
            registration: RegistrationToken {
                token,
                expires_in_s: REGISTRATION_TOKEN_SECONDS,
            },
        })
    }

    /// `POST /v1/auth/entra {token, device_id}` for an active device of the account: a new
    /// session for it and new tokens; its earlier refresh tokens are revoked.
    pub async fn sign_in_entra_device(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
        device_id: Uuid,
    ) -> Result<Tokens, AuthError> {
        let max_session = self.max_session()?;
        let account = self.entra_account(identity, handle).await?;
        let session = Some(unix_now() + max_session);
        let mut tx = self.pool.begin().await?;
        // Locks the device row first, like every change to a device's tokens.
        let active = sqlx::query(
            "SELECT 1 FROM auth.device WHERE device_id = $1 AND user_id = $2 \
             AND revoked_at IS NULL FOR UPDATE",
        )
        .bind(device_id)
        .bind(account.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if active.is_none() {
            return Err(AuthError::DeviceNotFound);
        }
        set_session(&mut tx, device_id, session).await?;
        revoke_refresh_tokens(&mut tx, device_id).await?;
        let tokens = self
            .issue(&mut tx, account.user_id, device_id, session)
            .await?;
        tx.commit().await?;
        Ok(tokens)
    }

    /// The configured maximum session age in seconds; `Unauthorized` when Entra ID
    /// sign-in is not offered.
    fn max_session(&self) -> Result<i64, AuthError> {
        self.entra()
            .map(|entra| entra.max_session().as_secs() as i64)
            .ok_or(AuthError::Unauthorized)
    }

    /// The account of `identity`, created if it has none. A concurrent first sign-in of
    /// the same identity may win the race once; then its account is the one.
    async fn entra_account(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
    ) -> Result<EntraAccount, AuthError> {
        for _ in 0..2 {
            if let Some(found) = self.find_entra(identity).await? {
                return Ok(found);
            }
            if let Some(created) = self.create_entra(identity, handle).await? {
                return Ok(created);
            }
        }
        Err(AuthError::Unavailable)
    }

    /// The identity's account, recording this sign-in and the address the token carried.
    async fn find_entra(
        &self,
        identity: &EntraIdentity,
    ) -> Result<Option<EntraAccount>, AuthError> {
        let row = sqlx::query(
            "UPDATE auth.external_identity SET last_sign_in_at = now(), email = $3 \
             WHERE provider = 'entra' AND issuer = $1 AND subject = $2 \
             RETURNING identity_id, user_id",
        )
        .bind(identity.tenant_id.to_string())
        .bind(identity.object_id.to_string())
        .bind(identity.email.as_deref())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| EntraAccount {
            identity_id: row.get(0),
            user_id: row.get(1),
        }))
    }

    /// A new account for `identity`; `None` if another sign-in created one meanwhile
    /// (this transaction is then rolled back, the account it made included).
    async fn create_entra(
        &self,
        identity: &EntraIdentity,
        handle: Option<&str>,
    ) -> Result<Option<EntraAccount>, AuthError> {
        let requested = handle.map(rules::handle).transpose()?;
        let user_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let mut created = false;
        for _ in 0..HANDLE_ATTEMPTS {
            let candidate = requested.clone().unwrap_or_else(generated_handle);
            let display_name = identity
                .name
                .as_deref()
                .and_then(|name| rules::display_name(name).ok())
                .unwrap_or_else(|| candidate.clone());
            // Active from the start: email_verified_at is the first sign-in (spec D5).
            let inserted = sqlx::query(
                "INSERT INTO auth.user_account \
                 (user_id, email, handle, display_name, password_hash, email_verified_at) \
                 VALUES ($1, NULL, $2, $3, NULL, now()) \
                 ON CONFLICT (handle) DO NOTHING RETURNING 1",
            )
            .bind(user_id)
            .bind(&candidate)
            .bind(&display_name)
            .fetch_optional(&mut *tx)
            .await?;
            if inserted.is_some() {
                created = true;
                break;
            }
            if requested.is_some() {
                return Err(AuthError::HandleTaken);
            }
        }
        if !created {
            return Err(AuthError::Unavailable);
        }
        let identity_id = Uuid::now_v7();
        let linked = sqlx::query(
            "INSERT INTO auth.external_identity \
             (identity_id, provider, issuer, subject, user_id, email) \
             VALUES ($1, 'entra', $2, $3, $4, $5) \
             ON CONFLICT (provider, issuer, subject) DO NOTHING RETURNING 1",
        )
        .bind(identity_id)
        .bind(identity.tenant_id.to_string())
        .bind(identity.object_id.to_string())
        .bind(user_id)
        .bind(identity.email.as_deref())
        .fetch_optional(&mut *tx)
        .await?;
        if linked.is_none() {
            tx.rollback().await?;
            return Ok(None);
        }
        outbox::write(&mut tx, &Event::UserVerified { user_id }).await?;
        tx.commit().await?;
        tracing::info!(
            target: "security",
            event = "auth.entra.provisioned",
            %user_id,
            tenant = %identity.tenant_id,
            "an account was created for an Entra ID user"
        );
        Ok(Some(EntraAccount {
            user_id,
            identity_id,
        }))
    }
}

/// `u` and 10 random lower-case letters and digits: a valid handle nobody chose.
fn generated_handle() -> String {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 10];
    rand::rng().fill_bytes(&mut bytes);
    let mut handle = String::from("u");
    handle.extend(
        bytes
            .iter()
            .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()])),
    );
    handle
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn generated_handles_are_valid_and_differ() {
        let (first, second) = (generated_handle(), generated_handle());
        assert_eq!(rules::handle(&first).as_deref(), Ok(first.as_str()));
        assert_eq!(first.len(), 11);
        assert_ne!(first, second);
    }
}
