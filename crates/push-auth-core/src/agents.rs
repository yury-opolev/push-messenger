//! Owner consent, bounded reservations, and agent deletion that releases its identity.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::devices::{Caller, revoke_devices};
use crate::error::AuthError;
use crate::external::ExternalRegistration;
use crate::identity::{AGENT_KEY_PROVIDER, AGENT_PROVIDERS, ENTRA_APP_PROVIDER, Principal};
use crate::outbox::{self, Event};
use crate::rules;
use crate::service::Auth;
use crate::tokens::{new_refresh_token, refresh_hash};

pub(crate) const ENROLLMENT_SECONDS: i64 = 600;

// Deliberately no Debug: this is the only copy of the enrollment secret.
pub(crate) struct Reservation {
    pub user_id: Uuid,
    pub enrollment_code: String,
    pub expires_at: DateTime<Utc>,
}

/// Global expiry commits before any operation takes owner, account or device locks.
/// Keeping its agent-row locks in a caller's transaction would invert that order.
/// Accepting a pool, not a connection, prevents cleanup inside such a transaction.
pub(crate) async fn expire(pool: &PgPool) -> Result<(), AuthError> {
    sqlx::query(
        "DELETE FROM auth.user_account a USING auth.agent_enrollment e \
         WHERE a.user_id = e.user_id AND a.email_verified_at IS NULL \
         AND e.expires_at <= clock_timestamp()",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn owner(connection: &mut PgConnection, caller: Caller) -> Result<(), AuthError> {
    // Linked-only app accounts predate kind; their default `user` is not a human. The
    // provider list is `AGENT_PROVIDERS`.
    let human: Option<bool> = sqlx::query_scalar(
        "SELECT kind = 'user' AND email_verified_at IS NOT NULL AND NOT EXISTS ( \
         SELECT 1 FROM auth.external_identity e WHERE e.user_id = a.user_id \
         AND e.provider IN ('entra-app', 'agent-key')) FROM auth.user_account a \
         WHERE user_id = $1 FOR UPDATE",
    )
    .bind(caller.user_id)
    .fetch_optional(&mut *connection)
    .await?;
    if human != Some(true) {
        return Err(AuthError::EnrollmentNotPermitted);
    }
    // The consent must be from an actual still-active human device, not a supplied UUID.
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM auth.device WHERE user_id = $1 AND device_id = $2 \
         AND revoked_at IS NULL)",
    )
    .bind(caller.user_id)
    .bind(caller.device_id)
    .fetch_one(connection)
    .await?;
    if !active {
        return Err(AuthError::Unauthorized);
    }
    Ok(())
}

/// Moves every external identity of `agent` into `auth.released_identity` (spec R1-R3),
/// whatever its provider, so the identity can be enrolled again; the account keeps its
/// history. Call inside the transaction that disables the account; it returns the
/// released rows and the caller logs them after commit.
async fn release_identities(
    connection: &mut PgConnection,
    agent: Uuid,
) -> Result<Vec<Released>, AuthError> {
    let released: Vec<(String, String, String, Option<Uuid>)> = sqlx::query_as(
        "WITH gone AS (DELETE FROM auth.external_identity WHERE user_id = $1 \
         RETURNING identity_id, provider, issuer, subject, user_id, created_at), \
         kept AS (INSERT INTO auth.released_identity \
         (identity_id, provider, issuer, subject, user_id, created_at) \
         SELECT identity_id, provider, issuer, subject, user_id, created_at FROM gone \
         RETURNING provider, issuer, subject, user_id) \
         SELECT k.provider, k.issuer, k.subject, e.client_app_id FROM kept k \
         LEFT JOIN auth.agent_enrollment e USING (user_id) ORDER BY k.provider, k.issuer, k.subject",
    )
    .bind(agent)
    .fetch_all(connection)
    .await?;
    Ok(released
        .into_iter()
        .map(|(provider, issuer, subject, client_app_id)| Released {
            provider,
            issuer,
            subject,
            client_app_id,
        })
        .collect())
}

/// An external identity moved into `auth.released_identity` by [`release_identities`].
struct Released {
    provider: String,
    issuer: String,
    subject: String,
    client_app_id: Option<Uuid>,
}

impl Auth {
    pub(crate) async fn reserve_agent(
        &self,
        caller: Caller,
        handle: &str,
        display_name: &str,
    ) -> Result<Reservation, AuthError> {
        let tenants = self.app_tenants();
        let keys = self.agent_key_offered();
        if tenants.is_none() && !keys {
            return Err(AuthError::EnrollmentNotPermitted);
        }
        let handle = rules::handle(handle)?;
        let display_name = rules::display_name(display_name)?;
        expire(&self.pool).await?;
        let mut tx = self.pool.begin().await?;
        owner(&mut tx, caller).await?;
        let mut tenant = None;
        if let Some(tenants) = tenants {
            let delegated: Option<String> = sqlx::query_scalar(
                "SELECT issuer FROM auth.external_identity WHERE user_id = $1 AND provider = 'entra'",
            )
            .bind(caller.user_id)
            .fetch_optional(&mut *tx)
            .await?;
            tenant = match delegated {
                Some(issuer) => issuer
                    .parse::<Uuid>()
                    .ok()
                    .filter(|id| tenants.contains(id)),
                None if tenants.len() == 1 => Some(tenants[0]),
                None => None,
            };
        }
        // Without a tenant only `agent-key` can redeem the reservation.
        if tenant.is_none() && !keys {
            return Err(AuthError::EnrollmentNotPermitted);
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auth.user_account WHERE owner_user_id = $1 AND disabled_at IS NULL",
        )
        .bind(caller.user_id)
        .fetch_one(&mut *tx)
        .await?;
        if count >= i64::from(self.agents_per_user) {
            return Err(AuthError::TooManyAgents);
        }
        let user_id = Uuid::now_v7();
        let inserted = sqlx::query(
            "INSERT INTO auth.user_account (user_id, handle, display_name, kind, owner_user_id) \
             VALUES ($1, $2, $3, 'agent', $4) ON CONFLICT (handle) DO NOTHING RETURNING 1",
        )
        .bind(user_id)
        .bind(handle)
        .bind(display_name)
        .bind(caller.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_none() {
            return Err(AuthError::HandleTaken);
        }
        let enrollment_code = new_refresh_token();
        let expires_at = sqlx::query_scalar(
            "INSERT INTO auth.agent_enrollment \
             (user_id, tenant_id, consent_device_id, code_hash, expires_at) \
             VALUES ($1, $2, $3, $4, clock_timestamp() + interval '600 seconds') RETURNING expires_at",
        )
        .bind(user_id)
        .bind(tenant)
        .bind(caller.device_id)
        .bind(refresh_hash(&enrollment_code).as_slice())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Reservation {
            user_id,
            enrollment_code,
            expires_at,
        })
    }

    pub(crate) async fn enroll_agent(
        &self,
        principal: &Principal,
        code: &str,
    ) -> Result<ExternalRegistration, AuthError> {
        let provider = principal.provider.as_str();
        if !AGENT_PROVIDERS.contains(&provider) || self.authenticator(provider).is_none() {
            return Err(AuthError::EnrollmentNotPermitted);
        }
        let principal = principal.checked(provider).ok_or(AuthError::Unauthorized)?;
        let mut tx = self.pool.begin().await?;
        // Identity lock precedes owner locks, serializing even redemptions of different
        // owners' codes for the same identity. The unique key remains the final authority.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "{}:{}:{}",
                principal.provider, principal.issuer, principal.subject
            ))
            .execute(&mut *tx)
            .await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auth.external_identity WHERE provider = $1 \
             AND issuer = $2 AND subject = $3)",
        )
        .bind(&principal.provider)
        .bind(&principal.issuer)
        .bind(&principal.subject)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            return Err(AuthError::IdentityInUse);
        }
        if self.agents_per_user == 0 {
            return Err(AuthError::EnrollmentNotPermitted);
        }
        let reservation: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT a.user_id, a.owner_user_id FROM auth.agent_enrollment e \
             JOIN auth.user_account a USING (user_id) WHERE e.code_hash = $1",
        )
        .bind(refresh_hash(code).as_slice())
        .fetch_optional(&mut *tx)
        .await?;
        let (user_id, owner_id) = reservation.ok_or(AuthError::EnrollmentInvalid)?;
        sqlx::query("SELECT 1 FROM auth.user_account WHERE user_id = $1 FOR UPDATE")
            .bind(owner_id)
            .fetch_one(&mut *tx)
            .await?;
        let pending: Option<(Option<Uuid>, bool)> = sqlx::query_as(
            "SELECT e.tenant_id, e.expires_at > clock_timestamp() \
             FROM auth.user_account a JOIN auth.agent_enrollment e USING (user_id) \
             WHERE a.user_id = $1 AND a.disabled_at IS NULL AND a.email_verified_at IS NULL \
             AND e.code_hash = $2 FOR UPDATE OF a",
        )
        .bind(user_id)
        .bind(refresh_hash(code).as_slice())
        .fetch_optional(&mut *tx)
        .await?;
        let (tenant, valid) = pending.ok_or(AuthError::EnrollmentInvalid)?;
        if !valid {
            return Err(AuthError::EnrollmentInvalid);
        }
        // For `agent-key` the owner's reservation and its code are the authority.
        if principal.provider == ENTRA_APP_PROVIDER
            && tenant.map(|t| t.to_string()).as_deref() != Some(principal.issuer.as_str())
        {
            return Err(AuthError::EnrollmentNotPermitted);
        }
        sqlx::query(
            "INSERT INTO auth.external_identity (identity_id, provider, issuer, subject, user_id) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(&principal.provider)
        .bind(&principal.issuer)
        .bind(&principal.subject)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE auth.user_account SET email_verified_at = now() WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        let azp = if principal.provider == ENTRA_APP_PROVIDER {
            principal
                .claims
                .get("azp")
                .and_then(|v| v.as_str())
                .and_then(|v| Uuid::parse_str(v).ok())
        } else {
            None
        };
        sqlx::query(
            "UPDATE auth.agent_enrollment SET code_hash = NULL, enrolled_at = now(), client_app_id = $2 \
             WHERE user_id = $1",
        )
        .bind(user_id).bind(azp).execute(&mut *tx).await?;
        outbox::write(
            &mut tx,
            &Event::AgentEnrolled {
                user_id,
                owner_user_id: owner_id,
            },
        )
        .await?;
        tx.commit().await?;
        tracing::info!(
            target: "security", event = "auth.agent.enrolled", %user_id,
            owner_user_id = %owner_id, provider = %principal.provider,
            tenant_id = tenant.map(tracing::field::display), client_app_id = ?azp,
            "an owned agent account was enrolled"
        );
        self.sign_in_external(&principal, None).await
    }

    pub(crate) async fn owned_agents(
        &self,
        caller: Caller,
    ) -> Result<Vec<serde_json::Value>, AuthError> {
        expire(&self.pool).await?;
        let mut tx = self.pool.begin().await?;
        owner(&mut tx, caller).await?;
        // The provider list is `AGENT_PROVIDERS`; a pending agent has no identity yet.
        let rows = sqlx::query(
            "SELECT a.user_id, a.handle::text, a.display_name, a.email_verified_at IS NOT NULL, \
             e.expires_at, e.client_app_id, c.provider, c.subject FROM auth.user_account a \
             JOIN auth.agent_enrollment e USING (user_id) \
             LEFT JOIN LATERAL (SELECT x.provider, x.subject FROM auth.external_identity x \
             WHERE x.user_id = a.user_id AND x.provider IN ('entra-app', 'agent-key') \
             ORDER BY x.created_at DESC LIMIT 1) c ON true \
             WHERE a.owner_user_id = $1 AND a.disabled_at IS NULL ORDER BY a.user_id",
        )
        .bind(caller.user_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut agents = Vec::new();
        for row in rows {
            let id: Uuid = row.get(0);
            let devices = sqlx::query(
                "SELECT device_id, name, platform, created_at, last_seen_at FROM auth.device \
                 WHERE user_id = $1 AND revoked_at IS NULL ORDER BY device_id",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
            let devices: Vec<_> = devices.iter().map(|d| serde_json::json!({
                "device_id": d.get::<Uuid,_>(0), "name": d.get::<String,_>(1),
                "platform": d.get::<String,_>(2), "created_at": d.get::<DateTime<Utc>,_>(3).to_rfc3339(),
                "last_seen_at": d.get::<Option<DateTime<Utc>>,_>(4).map(|at| at.to_rfc3339())
            })).collect();
            let credential: Option<String> = row.get(6);
            let key_thumbprint = (credential.as_deref() == Some(AGENT_KEY_PROVIDER))
                .then(|| row.get::<String, _>(7));
            agents.push(serde_json::json!({
                "user_id": id, "handle": row.get::<String,_>(1),
                "display_name": row.get::<String,_>(2), "kind": "agent",
                "owner_user_id": caller.user_id,
                "state": if row.get::<bool,_>(3) { "active" } else { "pending" },
                "expires_at": if row.get::<bool,_>(3) { None } else { Some(row.get::<DateTime<Utc>,_>(4).to_rfc3339()) },
                "client_app_id": row.get::<Option<Uuid>,_>(5), "credential": credential,
                "key_thumbprint": key_thumbprint, "devices": devices,
            }));
        }
        tx.commit().await?;
        Ok(agents)
    }

    pub(crate) async fn change_agent(
        &self,
        caller: Caller,
        agent: Uuid,
        handle: Option<&str>,
        name: Option<&str>,
        delete: bool,
    ) -> Result<(), AuthError> {
        let handle = handle.map(rules::handle).transpose()?;
        let name = name.map(rules::display_name).transpose()?;
        if !delete && handle.is_none() && name.is_none() {
            return Err(AuthError::Invalid(rules::Invalid {
                field: "body",
                reason: "give display_name, handle or both",
            }));
        }
        expire(&self.pool).await?;
        let mut tx = self.pool.begin().await?;
        owner(&mut tx, caller).await?;
        let found: Option<bool> = sqlx::query_scalar(
            "SELECT email_verified_at IS NOT NULL FROM auth.user_account \
             WHERE user_id = $1 AND owner_user_id = $2 AND kind = 'agent' \
             AND disabled_at IS NULL FOR UPDATE",
        )
        .bind(agent)
        .bind(caller.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        let active = found.ok_or(AuthError::AgentNotFound)?;
        let mut released = Vec::new();
        if delete {
            if active {
                sqlx::query("UPDATE auth.user_account SET disabled_at = now() WHERE user_id = $1")
                    .bind(agent)
                    .execute(&mut *tx)
                    .await?;
                let devices: Vec<(Uuid, Uuid)> = sqlx::query_as(
                    "SELECT user_id, device_id FROM auth.device WHERE user_id = $1 \
                     AND revoked_at IS NULL ORDER BY device_id FOR UPDATE",
                )
                .bind(agent)
                .fetch_all(&mut *tx)
                .await?;
                revoke_devices(&mut tx, &devices).await?;
                released = release_identities(&mut tx, agent).await?;
            } else {
                sqlx::query("DELETE FROM auth.user_account WHERE user_id = $1")
                    .bind(agent)
                    .execute(&mut *tx)
                    .await?;
            }
        } else {
            let result = sqlx::query(
                "UPDATE auth.user_account SET handle = coalesce($2, handle), \
                 display_name = coalesce($3, display_name) WHERE user_id = $1",
            )
            .bind(agent)
            .bind(handle)
            .bind(name)
            .execute(&mut *tx)
            .await;
            match result {
                Err(e) if crate::service::is_violation(&e, "user_account_handle_key") => {
                    return Err(AuthError::HandleTaken);
                }
                Err(e) => return Err(e.into()),
                Ok(_) => {}
            }
        }
        tx.commit().await?;
        for row in released {
            tracing::info!(
                target: "security", event = "auth.agent.identity_released", user_id = %agent,
                owner_user_id = %caller.user_id, provider = %row.provider, issuer = %row.issuer,
                subject = %row.subject, client_app_id = ?row.client_app_id,
                "a deleted agent's identity was released"
            );
        }
        Ok(())
    }
}
