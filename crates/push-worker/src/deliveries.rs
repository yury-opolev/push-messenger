//! Delivery webhooks (spec §9): verified, recorded once in `worker.email_event`, and hard
//! bounces and complaints added to `worker.email_suppression`.

use std::sync::Arc;
use std::time::SystemTime;

use push_email::{DeliveryKind, WebhookError, WebhookVerifier};
use serde_json::Value;
use sqlx::PgPool;

/// What became of a webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// A new delivery event, recorded; `suppressed` addresses now get no more email.
    Recorded { suppressed: u64 },
    /// The same webhook again: nothing changed.
    Replay,
    /// A kind of event nothing acts on.
    Ignored,
    /// Signed by the provider, but not a delivery event we can read: logged and dropped.
    /// It is acknowledged all the same, since the provider would send it again for days.
    Malformed,
}

/// Why a webhook was not taken.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// Unsigned, forged or too old: never worth sending again.
    #[error(transparent)]
    Refused(#[from] WebhookError),
    /// The worker's database did not answer: the provider should send it again.
    #[error("the worker's database did not answer")]
    Database(#[from] sqlx::Error),
}

/// Takes the provider's delivery webhooks.
#[derive(Clone)]
pub struct Deliveries {
    pool: PgPool,
    verifier: Arc<dyn WebhookVerifier>,
}

impl std::fmt::Debug for Deliveries {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Deliveries").finish_non_exhaustive()
    }
}

impl Deliveries {
    pub fn new(pool: PgPool, verifier: Arc<dyn WebhookVerifier>) -> Self {
        Self { pool, verifier }
    }

    /// Verifies a webhook (headers through `header`, the raw `body`) and records its
    /// delivery event once: its id is the webhook's, the same on every delivery of it. A
    /// hard bounce or a complaint suppresses the recipients, the ones the event names and
    /// the one the email was sent to.
    pub async fn receive(
        &self,
        header: &(dyn Fn(&str) -> Option<String> + Sync),
        body: &[u8],
    ) -> Result<Received, DeliveryError> {
        let id = self.verifier.verify(header, body, SystemTime::now())?;
        let malformed = || {
            tracing::warn!(
                webhook_id = %id,
                "a signed webhook is not a delivery event we can read; dropped"
            );
            Ok(Received::Malformed)
        };
        let (event, raw) = match (
            self.verifier.parse(body),
            serde_json::from_slice::<Value>(body),
        ) {
            (Ok(None), _) => return Ok(Received::Ignored),
            (Ok(Some(event)), Ok(raw)) => (event, raw),
            _ => return malformed(),
        };
        let (kind, reason) = match event.kind {
            DeliveryKind::Delivered => (1_i16, None),
            DeliveryKind::Bounced { .. } => (2, Some(1_i16)),
            DeliveryKind::Complained => (3, Some(2)),
        };
        let mut tx = self.pool.begin().await?;
        let recorded = sqlx::query(
            "INSERT INTO worker.email_event \
             (event_id, provider_message_id, kind, occurred_at, raw) \
             VALUES ($1, $2, $3, coalesce($4, now()), $5) ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(&id)
        .bind(&event.provider_message_id)
        .bind(kind)
        .bind(event.occurred_at)
        .bind(&raw)
        .execute(&mut *tx)
        .await?;
        if recorded.rows_affected() == 0 {
            return Ok(Received::Replay);
        }
        let mut suppressed = 0;
        if let (true, Some(reason)) = (event.suppresses(), reason) {
            suppressed += sqlx::query(
                "INSERT INTO worker.email_suppression (address, reason) \
                 SELECT address, $2 FROM UNNEST($1::text[]) AS address \
                 ON CONFLICT (address) DO NOTHING",
            )
            .bind(&event.recipients)
            .bind(reason)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            suppressed += sqlx::query(
                "INSERT INTO worker.email_suppression (address, reason) \
                 SELECT to_address, $2 FROM worker.email_message \
                 WHERE provider_message_id = $1 ON CONFLICT (address) DO NOTHING",
            )
            .bind(&event.provider_message_id)
            .bind(reason)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        tracing::info!(kind, suppressed, "delivery event recorded");
        Ok(Received::Recorded { suppressed })
    }
}
