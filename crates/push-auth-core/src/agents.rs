//! Owner consent, bounded reservations and permanent app identity tombstones.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::devices::{Caller, revoke_devices};
use crate::error::AuthError;
use crate::external::ExternalRegistration;
use crate::identity::{ENTRA_APP_PROVIDER, Principal};
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

/// All enrollment operations lock human owner, then agent account, then devices.
/// Expiry only deletes unverified agent rows (never human owner rows).
pub(crate) async fn expire(connection: &mut PgConnection) -> Result<(), AuthError> {
    sqlx::query(
        "DELETE FROM auth.user_account a USING auth.agent_enrollment e \
         WHERE a.user_id = e.user_id AND a.email_verified_at IS NULL \
         AND e.expires_at <= clock_timestamp()",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn owner(connection: &mut PgConnection, caller: Caller) -> Result<(), AuthError> {
    let human: Option<bool> = sqlx::query_scalar(
        "SELECT kind = 'user' AND email_verified_at IS NOT NULL FROM auth.user_account \
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

impl Auth {
    pub(crate) async fn reserve_agent(
        &self,
        caller: Caller,
        handle: &str,
        display_name: &str,
    ) -> Result<Reservation, AuthError> {
        let tenants = self
            .app_tenants()
            .ok_or(AuthError::EnrollmentNotPermitted)?;
        let handle = rules::handle(handle)?;
        let display_name = rules::display_name(display_name)?;
        let mut tx = self.pool.begin().await?;
        owner(&mut tx, caller).await?;
        expire(&mut tx).await?;
        let delegated: Option<String> = sqlx::query_scalar(
            "SELECT issuer FROM auth.external_identity WHERE user_id = $1 AND provider = 'entra'",
        )
        .bind(caller.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        let tenant = match delegated {
            Some(issuer) => issuer
                .parse::<Uuid>()
                .ok()
                .filter(|id| tenants.contains(id)),
            None if tenants.len() == 1 => Some(tenants[0]),
            None => None,
        }
        .ok_or(AuthError::EnrollmentNotPermitted)?;
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
        if principal.provider != ENTRA_APP_PROVIDER || self.app_tenants().is_none() {
            return Err(AuthError::EnrollmentNotPermitted);
        }
        let principal = principal
            .checked(ENTRA_APP_PROVIDER)
            .ok_or(AuthError::Unauthorized)?;
        let mut tx = self.pool.begin().await?;
        // Identity lock precedes owner locks, serializing even redemptions of different
        // owners' codes for the same app. The unique key remains the final authority.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "entra-app:{}:{}",
                principal.issuer, principal.subject
            ))
            .execute(&mut *tx)
            .await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auth.external_identity WHERE provider = 'entra-app' \
             AND issuer = $1 AND subject = $2)",
        )
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
        let pending: Option<(Uuid, bool)> = sqlx::query_as(
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
        if tenant.to_string() != principal.issuer {
            return Err(AuthError::EnrollmentNotPermitted);
        }
        sqlx::query(
            "INSERT INTO auth.external_identity (identity_id, provider, issuer, subject, user_id) \
             VALUES ($1, 'entra-app', $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(&principal.issuer)
        .bind(&principal.subject)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE auth.user_account SET email_verified_at = now() WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        let azp = principal
            .claims
            .get("azp")
            .and_then(|v| v.as_str())
            .and_then(|v| Uuid::parse_str(v).ok());
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
            owner_user_id = %owner_id, tenant_id = %tenant, client_app_id = ?azp,
            "an owned app account was enrolled"
        );
        self.sign_in_external(&principal, None).await
    }

    pub(crate) async fn owned_agents(
        &self,
        caller: Caller,
    ) -> Result<Vec<serde_json::Value>, AuthError> {
        let mut tx = self.pool.begin().await?;
        owner(&mut tx, caller).await?;
        expire(&mut tx).await?;
        let rows = sqlx::query(
            "SELECT a.user_id, a.handle::text, a.display_name, a.email_verified_at IS NOT NULL, \
             e.expires_at, e.client_app_id FROM auth.user_account a \
             JOIN auth.agent_enrollment e USING (user_id) \
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
            agents.push(serde_json::json!({
                "user_id": id, "handle": row.get::<String,_>(1),
                "display_name": row.get::<String,_>(2), "kind": "agent",
                "owner_user_id": caller.user_id,
                "state": if row.get::<bool,_>(3) { "active" } else { "pending" },
                "expires_at": if row.get::<bool,_>(3) { None } else { Some(row.get::<DateTime<Utc>,_>(4).to_rfc3339()) },
                "client_app_id": row.get::<Option<Uuid>,_>(5), "devices": devices,
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
        let mut tx = self.pool.begin().await?;
        owner(&mut tx, caller).await?;
        expire(&mut tx).await?;
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
        Ok(())
    }
}
