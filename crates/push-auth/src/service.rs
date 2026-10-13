//! The auth operations (spec §5.1–§5.4), over the `auth` schema.
//!
//! Every change and the outbox event announcing it commit in one transaction. Password
//! hashing runs on blocking threads behind a [`HashGate`]: at most `hash_concurrency`
//! at a time, because each hash takes tens of milliseconds and 19 MiB of memory, and at
//! most `hash_queue` waiting, each for at most `hash_wait`, so a flood fails fast with a
//! retryable `unavailable`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::AuthError;
use crate::links::{LinkClaims, LinkError, LinkSigner, Purpose, password_stamp};
use crate::outbox::{self, Email, Event, LINK_MINUTES};
use crate::password::{Checked, HashGate, Passwords};
use crate::rules;
use crate::tokens::AccessTokens;

/// How long a registration token is valid, in seconds (spec §5.1: 5 minutes).
pub const REGISTRATION_TOKEN_SECONDS: i64 = 5 * 60;
/// How long an email link is valid, in seconds (spec §5.1: 30 minutes).
pub const LINK_SECONDS: i64 = LINK_MINUTES as i64 * 60;

/// What the service is built from.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The public base URL that email links start with, such as
    /// `https://api.example.com`; never taken from a request's `Host` header.
    pub public_url: String,
    pub passwords: Passwords,
    pub links: LinkSigner,
    pub tokens: AccessTokens,
    /// Most password hashes computed at once.
    pub hash_concurrency: usize,
    /// Most requests waiting for a hash slot; more are refused at once.
    pub hash_queue: usize,
    /// Longest wait for a hash slot.
    pub hash_wait: Duration,
}

/// A new account.
#[derive(Debug, Clone)]
pub struct Signup {
    pub email: String,
    pub password: String,
    pub handle: String,
    pub display_name: String,
}

/// Proof that an account may register one device (spec §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationToken {
    pub token: String,
    pub expires_in_s: i64,
}

/// The auth service.
#[derive(Debug, Clone)]
pub struct Auth {
    pub(crate) pool: PgPool,
    passwords: Arc<Passwords>,
    hashing: Arc<HashGate>,
    pub(crate) links: LinkSigner,
    pub(crate) tokens: AccessTokens,
    public_url: String,
}

/// Seconds since the Unix epoch.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64
}

fn link_error(error: LinkError) -> AuthError {
    match error {
        LinkError::Expired => AuthError::LinkExpired,
        LinkError::Malformed | LinkError::Forged | LinkError::WrongPurpose => {
            AuthError::LinkInvalid
        }
    }
}

impl Auth {
    /// The service on `pool`, a pool of the migrated `push_auth` database.
    pub fn new(pool: PgPool, settings: Settings) -> Self {
        Self {
            pool,
            passwords: Arc::new(settings.passwords),
            hashing: Arc::new(HashGate::new(
                settings.hash_concurrency,
                settings.hash_queue,
                settings.hash_wait,
            )),
            links: settings.links,
            tokens: settings.tokens,
            public_url: settings.public_url.trim_end_matches('/').to_owned(),
        }
    }

    /// The access-token issuer, for the JWKS.
    pub fn tokens(&self) -> &AccessTokens {
        &self.tokens
    }

    /// The database pool, for readiness checks.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // The permit moves into the blocking closure: a cancelled request must not free the
    // slot while its hash is still running.
    async fn hash(&self, password: String) -> Result<String, AuthError> {
        let permit = self
            .hashing
            .enter()
            .await
            .map_err(|_| AuthError::Unavailable)?;
        let passwords = Arc::clone(&self.passwords);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            passwords.hash(&password)
        })
        .await
        .map_err(|_| AuthError::Unavailable)
    }

    /// Checks `password` against `stored`, or spends the same time when there is no
    /// account (`stored` is `None`), so both answers take equally long.
    pub(crate) async fn check_password(
        &self,
        password: String,
        stored: Option<String>,
    ) -> Result<Checked, AuthError> {
        let permit = self
            .hashing
            .enter()
            .await
            .map_err(|_| AuthError::Unavailable)?;
        let passwords = Arc::clone(&self.passwords);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match stored {
                Some(stored) => passwords.check(&password, &stored),
                None => {
                    passwords.check_nothing(&password);
                    Checked::Wrong
                }
            }
        })
        .await
        .map_err(|_| AuthError::Unavailable)
    }

    fn verify_link(&self, user_id: Uuid, email: &str, password_hash: &str) -> String {
        let token = self.links.sign(&LinkClaims {
            sub: user_id,
            purpose: Purpose::Verify,
            exp: unix_now() + LINK_SECONDS,
            email: Some(email.to_owned()),
            pw: Some(password_stamp(password_hash)),
        });
        format!("{}/v1/auth/verify?token={token}", self.public_url)
    }

    /// Creates an unverified account and writes its verification email to the outbox
    /// (spec §5.2). The answer is the same, in the same time, whether or not the email
    /// has an account. An existing unverified account starts over: the new password,
    /// handle and display name replace the old ones and a new link is sent, which the
    /// links sent before cannot outlive (they bind the old password hash) — otherwise
    /// whoever signed up first with someone else's address would own the account that
    /// person verifies. A verified account changes nothing and gets an "account exists"
    /// email. A handle held by any account, this email's own included, is reported as
    /// taken (handles are public through lookup anyway), so that answer never depends on
    /// the email either.
    pub async fn signup(&self, signup: Signup) -> Result<(), AuthError> {
        let email = rules::email(&signup.email)?;
        let handle = rules::handle(&signup.handle)?;
        let display_name = rules::display_name(&signup.display_name)?;
        rules::password(&signup.password)?;
        // Hashed before anything else, whether or not the account will be created.
        let password_hash = self.hash(signup.password).await?;

        let mut tx = self.pool.begin().await?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auth.user_account WHERE handle = $1::citext)",
        )
        .bind(&handle)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(AuthError::HandleTaken);
        }
        // One statement for a new account and for a new start of an unverified one, so
        // concurrent signups for one address cannot both insert. A verified account does
        // not match the WHERE, and no row comes back.
        let upserted = sqlx::query(
            "INSERT INTO auth.user_account (user_id, email, handle, display_name, password_hash) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (email) DO UPDATE SET handle = EXCLUDED.handle, \
             display_name = EXCLUDED.display_name, password_hash = EXCLUDED.password_hash \
             WHERE auth.user_account.email_verified_at IS NULL \
             RETURNING user_id, email::text",
        )
        .bind(Uuid::now_v7())
        .bind(&email)
        .bind(&handle)
        .bind(&display_name)
        .bind(&password_hash)
        .fetch_optional(&mut *tx)
        .await;
        let upserted = match upserted {
            Ok(row) => row,
            Err(error) if is_violation(&error, "user_account_handle_key") => {
                return Err(AuthError::HandleTaken);
            }
            Err(error) => return Err(error.into()),
        };
        let event = match upserted {
            Some(account) => {
                let user_id: Uuid = account.get(0);
                let to: String = account.get(1);
                Event::EmailSend {
                    user_id,
                    email: Email::VerifyEmail {
                        link: self.verify_link(user_id, &to, &password_hash),
                        handle,
                    },
                    to,
                }
            }
            None => {
                let existing = sqlx::query(
                    "SELECT user_id, email::text, handle::text \
                     FROM auth.user_account WHERE email = $1::citext",
                )
                .bind(&email)
                .fetch_one(&mut *tx)
                .await?;
                Event::EmailSend {
                    user_id: existing.get(0),
                    to: existing.get(1),
                    email: Email::AccountExists {
                        handle: existing.get(2),
                    },
                }
            }
        };
        outbox::write(&mut tx, &event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Opens a verification link: marks the email verified and writes `user.verified`
    /// (spec §5.2). Opening it again changes nothing. It never signs the user in.
    pub async fn verify_email(&self, token: &str) -> Result<(), AuthError> {
        let claims = self
            .links
            .verify(token, Purpose::Verify, unix_now())
            .map_err(link_error)?;
        let (Some(email), Some(stamp)) = (claims.email, claims.pw) else {
            return Err(AuthError::LinkInvalid);
        };
        let mut tx = self.pool.begin().await?;
        // The link binds the address it was sent to and the password hash of that moment:
        // a link for an old address of the account (after a future email change), or one
        // sent before a new signup replaced the password, verifies nothing.
        let account = sqlx::query(
            "SELECT password_hash, email_verified_at IS NOT NULL FROM auth.user_account \
             WHERE user_id = $1 AND email = $2::citext FOR UPDATE",
        )
        .bind(claims.sub)
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account) = account else {
            return Err(AuthError::LinkInvalid);
        };
        let password_hash: String = account.get(0);
        if password_stamp(&password_hash) != stamp {
            return Err(AuthError::LinkInvalid);
        }
        if account.get::<bool, _>(1) {
            return Ok(());
        }
        sqlx::query("UPDATE auth.user_account SET email_verified_at = now() WHERE user_id = $1")
            .bind(claims.sub)
            .execute(&mut *tx)
            .await?;
        outbox::write(
            &mut tx,
            &Event::UserVerified {
                user_id: claims.sub,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sends the verification email again if `email` has an unverified account. The
    /// answer is the same either way.
    pub async fn resend_verification(&self, email: &str) -> Result<(), AuthError> {
        let email = rules::email(email)?;
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT user_id, email::text, handle::text, password_hash FROM auth.user_account \
             WHERE email = $1::citext AND email_verified_at IS NULL",
        )
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(account) = account {
            let user_id: Uuid = account.get(0);
            let to: String = account.get(1);
            let password_hash: String = account.get(3);
            let event = Event::EmailSend {
                user_id,
                email: Email::VerifyEmail {
                    link: self.verify_link(user_id, &to, &password_hash),
                    handle: account.get(2),
                },
                to,
            };
            outbox::write(&mut tx, &event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Checks an email and password; the account's id and current password hash if they
    /// match and the email is verified. An unknown email and a wrong password are the
    /// same error and take the same time. A hash made with outdated parameters is
    /// replaced.
    pub(crate) async fn sign_in(
        &self,
        email: &str,
        password: &str,
    ) -> Result<(Uuid, String), AuthError> {
        let Ok(email) = rules::email(email) else {
            return Err(AuthError::InvalidCredentials);
        };
        let account = sqlx::query(
            "SELECT user_id, password_hash, email_verified_at IS NOT NULL \
             FROM auth.user_account WHERE email = $1::citext",
        )
        .bind(&email)
        .fetch_optional(&self.pool)
        .await?;
        let stored: Option<String> = account.as_ref().map(|row| row.get(1));
        let checked = self
            .check_password(password.to_owned(), stored.clone())
            .await?;
        let (Some(account), Some(stored), Checked::Right { rehash }) = (account, stored, checked)
        else {
            return Err(AuthError::InvalidCredentials);
        };
        let user_id: Uuid = account.get(0);
        // Before any rehash: a new hash changes `pw` and would void pending links.
        if !account.get::<bool, _>(2) {
            return Err(AuthError::EmailNotVerified);
        }
        let mut current = stored;
        if rehash {
            let new_hash = self.hash(password.to_owned()).await?;
            // Only if the password did not change meanwhile.
            let replaced = sqlx::query(
                "UPDATE auth.user_account SET password_hash = $3 \
                 WHERE user_id = $1 AND password_hash = $2",
            )
            .bind(user_id)
            .bind(&current)
            .bind(&new_hash)
            .execute(&self.pool)
            .await?;
            if replaced.rows_affected() == 1 {
                current = new_hash;
            }
        }
        Ok((user_id, current))
    }

    /// `POST /v1/auth/login {email, password}`: a registration token, which can only
    /// register a device (spec §5.3). It binds the password hash, so a password reset
    /// makes it worthless.
    pub async fn login(&self, email: &str, password: &str) -> Result<RegistrationToken, AuthError> {
        let (user_id, password_hash) = self.sign_in(email, password).await?;
        let token = self.links.sign(&LinkClaims {
            sub: user_id,
            purpose: Purpose::Register,
            exp: unix_now() + REGISTRATION_TOKEN_SECONDS,
            email: None,
            pw: Some(password_stamp(&password_hash)),
        });
        Ok(RegistrationToken {
            token,
            expires_in_s: REGISTRATION_TOKEN_SECONDS,
        })
    }

    /// Writes a password-reset email if `email` has an account (spec §5.4). The link
    /// binds a fingerprint of the current password hash. The answer is the same either
    /// way.
    pub async fn forgot_password(&self, email: &str) -> Result<(), AuthError> {
        let email = rules::email(email)?;
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT user_id, email::text, password_hash FROM auth.user_account WHERE email = $1::citext",
        )
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(account) = account {
            let user_id: Uuid = account.get(0);
            let token = self.links.sign(&LinkClaims {
                sub: user_id,
                purpose: Purpose::Reset,
                exp: unix_now() + LINK_SECONDS,
                email: None,
                pw: Some(password_stamp(account.get(2))),
            });
            let link = format!("{}/v1/auth/password/reset?token={token}", self.public_url);
            let event = Event::EmailSend {
                user_id,
                to: account.get(1),
                email: Email::ResetPassword { link },
            };
            outbox::write(&mut tx, &event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Opens a reset link: sets the new password, revokes every refresh token of the
    /// account and writes `user.password_reset`, on which every device is kicked
    /// (spec §5.4). Setting the password changes the stamp, so the link works once. The
    /// link reached the address, so the email counts as verified from now on; if it was
    /// not, `user.verified` is written too.
    pub async fn reset_password(&self, token: &str, password: &str) -> Result<(), AuthError> {
        let claims = self
            .links
            .verify(token, Purpose::Reset, unix_now())
            .map_err(link_error)?;
        rules::password(password)?;
        let new_hash = self.hash(password.to_owned()).await?;
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT password_hash, email_verified_at IS NULL FROM auth.user_account \
             WHERE user_id = $1 FOR UPDATE",
        )
        .bind(claims.sub)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account) = account else {
            return Err(AuthError::LinkInvalid);
        };
        let current: String = account.get(0);
        if claims.pw.as_deref() != Some(password_stamp(&current).as_str()) {
            return Err(AuthError::LinkInvalid);
        }
        sqlx::query(
            "UPDATE auth.user_account SET password_hash = $2, \
             email_verified_at = coalesce(email_verified_at, now()) WHERE user_id = $1",
        )
        .bind(claims.sub)
        .bind(&new_hash)
        .execute(&mut *tx)
        .await?;
        // Device rows first, in a fixed order, like every other change to a device's tokens.
        sqlx::query(
            "SELECT device_id FROM auth.device WHERE user_id = $1 ORDER BY device_id FOR UPDATE",
        )
        .bind(claims.sub)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE auth.refresh_token SET revoked_at = now() WHERE revoked_at IS NULL \
             AND device_id IN (SELECT device_id FROM auth.device WHERE user_id = $1)",
        )
        .bind(claims.sub)
        .execute(&mut *tx)
        .await?;
        if account.get::<bool, _>(1) {
            outbox::write(
                &mut tx,
                &Event::UserVerified {
                    user_id: claims.sub,
                },
            )
            .await?;
        }
        outbox::write(
            &mut tx,
            &Event::UserPasswordReset {
                user_id: claims.sub,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// DEV ONLY: the `email.send` payloads waiting in the outbox for the address `to`,
    /// newest first (at most 20). Until the worker sends email (Plan 6), this is how a
    /// local cluster and the smoke test read their verification links.
    pub async fn dev_mailbox(&self, to: &str) -> Result<Vec<serde_json::Value>, AuthError> {
        let emails = sqlx::query_scalar(
            "SELECT payload FROM auth.outbox WHERE kind = 'email.send' \
             AND lower(payload->>'to') = lower($1) \
             ORDER BY created_at DESC, event_id DESC LIMIT 20",
        )
        .bind(to.trim())
        .fetch_all(&self.pool)
        .await?;
        Ok(emails)
    }
}

/// Whether `error` violates the constraint `name`.
pub(crate) fn is_violation(error: &sqlx::Error, name: &str) -> bool {
    error
        .as_database_error()
        .and_then(|database| database.constraint())
        == Some(name)
}
