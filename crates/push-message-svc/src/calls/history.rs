//! Call history in message-svc (spec §9): the rows of an ended call, the `Calls` API
//! auth-svc serves `GET /v1/calls` and `DELETE /v1/calls/{call_id}` with, the
//! `CallHistoryChanged` events and the retention purge. Never logs a call's content.

use std::sync::Arc;
use std::time::Duration;

use chrono::TimeDelta;
use push_domain::calls::{CallHistoryRow, HistoryCursor};
use push_domain::{Clock, Messaging, Notifier, Store, UserId};
use push_proto::id_bytes;
use push_proto::internal::v1::{
    CallHistoryCursor, CallHistoryEntry, CallHistoryPage, CallHistoryRequest, HideCallRequest,
    HideCallResponse, calls_server,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::RedisCalls;
use super::wire;
use crate::convert::{domain_error, from_micros, micros, parse_id};
use crate::metrics::Metrics;
use crate::notify::RedisNotifier;

/// How often the rows of an ended call are tried before the failure is logged.
const WRITE_ATTEMPTS: u32 = 3;
/// The pause before the second write attempt; it doubles after each attempt.
const WRITE_BACKOFF: Duration = Duration::from_millis(100);
/// How often the retention purge runs.
const PURGE_EVERY: Duration = Duration::from_secs(60 * 60);
/// How many rows one purge statement deletes.
pub const PURGE_BATCH: u32 = 1000;
/// How many batches one purge run deletes at most before it waits for the next run.
const MAX_PURGE_BATCHES: usize = 1000;

/// Stores call history and tells the users' devices it changed.
pub struct CallHistory<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
    redis: RedisCalls,
    notifier: RedisNotifier,
    metrics: Arc<Metrics>,
}

impl<S, C, N> CallHistory<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// `notifier` publishes the events (and reads the device registry through its Redis
    /// connection).
    pub fn new(
        messaging: Arc<Messaging<S, C, N>>,
        notifier: RedisNotifier,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            messaging,
            redis: notifier.calls(),
            notifier,
            metrics,
        }
    }

    /// Stores the rows of an ended call (retried; a failure is logged and counted: the call
    /// has ended either way), then sends `CallHistoryChanged` to each row's user.
    pub(crate) async fn record(&self, call: Uuid, rows: &[CallHistoryRow], ended_ms: i64) {
        let mut backoff = WRITE_BACKOFF;
        for attempt in 1..=WRITE_ATTEMPTS {
            match self.messaging.record_call(rows).await {
                Ok(()) => {
                    let users: Vec<UserId> = rows.iter().map(|row| row.user).collect();
                    self.changed(&users, Some(ended_ms)).await;
                    return;
                }
                Err(error) if attempt == WRITE_ATTEMPTS => {
                    tracing::warn!(
                        call_id = %call,
                        %error,
                        "the call history of an ended call could not be stored"
                    );
                    self.metrics.history_write_failed();
                }
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
            }
        }
    }

    /// Sends `CallHistoryChanged` to the live calls-active devices of each of `users`. A
    /// failure is logged: the devices refresh their history when they reconnect.
    async fn changed(&self, users: &[UserId], latest_ended_ms: Option<i64>) {
        let sent = async {
            let now = self.redis.now().await?;
            let mut messages = Vec::new();
            for user in users {
                let devices = self.redis.live_devices(*user, now).await?;
                if !devices.is_empty() {
                    messages.push(wire::history_changed(*user, &devices, latest_ended_ms));
                }
            }
            self.notifier.publish_raw(&messages).await
        };
        if let Err(error) = sent.await {
            tracing::warn!(%error, "could not publish CallHistoryChanged");
        }
    }

    /// Deletes the rows of calls that ended more than `days` ago, in batches of
    /// [`PURGE_BATCH`], until none is left (or [`MAX_PURGE_BATCHES`] were deleted).
    /// Returns how many rows were deleted.
    pub async fn purge(&self, days: u32) -> Result<u64, push_domain::DomainError> {
        let keep = TimeDelta::days(i64::from(days));
        let mut total = 0;
        for _ in 0..MAX_PURGE_BATCHES {
            let deleted = self.messaging.purge_call_history(keep, PURGE_BATCH).await?;
            total += deleted;
            self.metrics.history_deleted(deleted);
            if deleted < u64::from(PURGE_BATCH) {
                break;
            }
        }
        Ok(total)
    }

    /// Purges every hour (the first run at once) until `shutdown`.
    pub async fn run_retention(&self, days: u32, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(PURGE_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            match self.purge(days).await {
                Ok(0) => {}
                Ok(deleted) => tracing::info!(deleted, days, "call history purged"),
                Err(error) => {
                    tracing::warn!(%error, "the call history purge failed; it runs again later");
                }
            }
        }
    }
}

impl<S, C, N> CallHistory<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    /// [`Self::record`] in a task of its own, so neither the ending frame's RPC deadline nor
    /// a sweep waits for the database or can cancel the write.
    pub(crate) fn record_in_background(
        self: &Arc<Self>,
        call: Uuid,
        rows: Vec<CallHistoryRow>,
        ended_ms: i64,
    ) {
        let history = Arc::clone(self);
        tokio::spawn(async move { history.record(call, &rows, ended_ms).await });
    }
}

/// The `Calls` service: auth-svc's call history calls. Guarded by auth-svc's internal
/// token, as `Directory` is.
pub struct CallHistoryApi<S, C, N> {
    history: Arc<CallHistory<S, C, N>>,
}

impl<S, C, N> CallHistoryApi<S, C, N> {
    pub fn new(history: Arc<CallHistory<S, C, N>>) -> Self {
        Self { history }
    }
}

#[tonic::async_trait]
impl<S, C, N> calls_server::Calls for CallHistoryApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn history(
        &self,
        request: Request<CallHistoryRequest>,
    ) -> Result<Response<CallHistoryPage>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let before = request.before.as_ref().map(cursor).transpose()?;
        let since = request.since.as_ref().map(cursor).transpose()?;
        let rows = self
            .history
            .messaging
            .call_history(user, before, since, request.limit)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(CallHistoryPage {
            calls: rows.iter().map(entry).collect(),
        }))
    }

    async fn hide(
        &self,
        request: Request<HideCallRequest>,
    ) -> Result<Response<HideCallResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let call = parse_id(&request.call_id, "call_id", std::convert::identity)?;
        let hidden = self
            .history
            .messaging
            .hide_call(user, call)
            .await
            .map_err(domain_error)?;
        if hidden {
            self.history.changed(&[user], None).await;
        }
        Ok(Response::new(HideCallResponse {}))
    }
}

fn cursor(cursor: &CallHistoryCursor) -> Result<HistoryCursor, Status> {
    Ok(HistoryCursor {
        ended_at: from_micros(cursor.ended_at)?,
        call_id: parse_id(&cursor.call_id, "call_id", std::convert::identity)?,
    })
}

fn entry(row: &CallHistoryRow) -> CallHistoryEntry {
    CallHistoryEntry {
        call_id: id_bytes(row.call_id),
        peer_user_id: id_bytes(row.peer.0),
        direction: wire::direction(row.direction).into(),
        started_at: micros(row.started_at),
        answered_at: row.answered_at.map(micros),
        ended_at: micros(row.ended_at),
        duration_s: row.duration_s,
        outcome: wire::end_reason(row.outcome).into(),
    }
}
