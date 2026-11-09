//! The call engine, in message-svc (spec D1): input checks, policy, the state machine of
//! push-domain over compare-and-set Redis state, publishing, the sweeper, audit and
//! metrics. Never logs SDP, candidates or addresses (spec D18, T10).

pub(crate) mod history;
mod redis;
mod wire;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use push_domain::calls::{
    CallEvent, CallRecord, CallState, Delivery, EndReason, Outcome, Refusal, Side, Timing,
    ended_rows, start, sync_view, transition,
};
use push_domain::{Clock, DeviceId, DomainError, Messaging, Notifier, Store, UserId};
use push_proto::error_status;
use push_proto::internal::v1::{CallDeviceRequest, CallReply, CallRequest, call_request};
use push_proto::v1::{self, CallSync, ErrorCode};
use tokio_util::sync::CancellationToken;
use tonic::{Code, Status};
use uuid::Uuid;

pub use self::history::{CallHistory, CallHistoryApi, PURGE_BATCH};
pub use self::redis::{Applied, DUE_KEY, RedisCalls, Write, call_key, devices_key, user_key};
use crate::config::CallsConfig;
use crate::convert::{domain_error, invalid, parse_id};
use crate::metrics::Metrics;
use crate::notify::RedisNotifier;

/// How often a frame retries after losing a compare-and-set race (spec §6).
const MAX_ATTEMPTS: usize = 5;
/// How many due calls one claim takes.
const CLAIM_MAX: u32 = 32;
/// How many full claims in a row one sweep makes before it yields to the next interval.
const MAX_FULL_CLAIMS: usize = 50;
/// How many claimed calls are ticked at a time.
const TICK_CONCURRENCY: usize = 16;
/// How long a claimed due call is leased to the claiming replica (spec D5).
const CLAIM_LEASE_MS: i64 = 5_000;
/// How often the sweeper claims due calls.
const SWEEP_EVERY: Duration = Duration::from_millis(500);
/// How long a ringing call's keys outlive its ring deadline (spec §5).
const RINGING_EXTRA_TTL_MS: i64 = 60_000;
/// How long an active call's keys live after each write (spec §5).
const ACTIVE_TTL_MS: i64 = 120_000;

impl CallsConfig {
    /// The state machine's durations for these settings.
    pub fn timing(&self) -> Timing {
        Timing {
            ring_ms: millis(self.ring),
            grace_ms: millis(self.grace),
            ..Timing::default()
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// The call RPCs of the `Messaging` service, object safe so `MessagingApi` stays generic
/// and the engine optional (`MessagingApi::with_calls`).
#[async_trait]
pub trait CallsApi: Send + Sync {
    /// `Messaging.Call`: one call frame of a calls-active device.
    async fn call(&self, request: CallRequest) -> Result<CallReply, Status>;
    /// `Messaging.CallAttach`: a calls-active connection identified.
    async fn attach(&self, request: CallDeviceRequest) -> Result<CallSync, Status>;
    /// `Messaging.CallDetach`: a calls-active connection closed.
    async fn detach(&self, request: CallDeviceRequest) -> Result<(), Status>;
}

/// The call engine: loads a call, runs the state machine, writes the result with
/// compare-and-set and publishes the frames before answering.
pub struct CallService<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
    redis: RedisCalls,
    notifier: RedisNotifier,
    timing: Timing,
    metrics: Arc<Metrics>,
    history: Arc<CallHistory<S, C, N>>,
}

impl<S, C, N> CallService<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    /// `messaging` answers who may call whom; `redis` keeps the call state; `notifier`
    /// publishes the frames and the history events; `timing` is usually
    /// [`CallsConfig::timing`]. Ended calls are written to the call history.
    pub fn new(
        messaging: Arc<Messaging<S, C, N>>,
        redis: RedisCalls,
        notifier: RedisNotifier,
        timing: Timing,
        metrics: Arc<Metrics>,
    ) -> Self {
        let history = Arc::new(CallHistory::new(
            Arc::clone(&messaging),
            notifier.clone(),
            Arc::clone(&metrics),
        ));
        Self {
            messaging,
            redis,
            notifier,
            timing,
            metrics,
            history,
        }
    }

    /// The call state store.
    pub fn redis(&self) -> &RedisCalls {
        &self.redis
    }

    /// Ticks the due calls: ring timeouts, grace ends and liveness checks. Claims again at
    /// once while a claim comes back full (at most [`MAX_FULL_CLAIMS`] times) and ticks
    /// [`TICK_CONCURRENCY`] calls at a time. Returns how many calls were claimed. A call
    /// that fails is logged and ticked again after its lease.
    pub async fn sweep_once(&self) -> Result<usize, Status> {
        self.sweep(&CancellationToken::new()).await
    }

    async fn sweep(&self, shutdown: &CancellationToken) -> Result<usize, Status> {
        let mut claimed = 0;
        for _ in 0..MAX_FULL_CLAIMS {
            let (_, due) = self
                .redis
                .claim(CLAIM_MAX, CLAIM_LEASE_MS)
                .await
                .map_err(redis_failed)?;
            claimed += due.len();
            let full = due.len() >= CLAIM_MAX as usize;
            futures_util::stream::iter(due)
                .for_each_concurrent(TICK_CONCURRENCY, |call| async move {
                    if let Err(status) = self.tick(call).await {
                        tracing::warn!(
                            call_id = %call,
                            error = status.message(),
                            "a due call could not be ticked; it is tried again after its lease"
                        );
                    }
                })
                .await;
            if !full || shutdown.is_cancelled() {
                break;
            }
        }
        Ok(claimed)
    }

    /// Sweeps every 500 ms until `shutdown`.
    pub async fn run_sweeper(&self, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(SWEEP_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            // A failure is logged; the next sweep tries again.
            let _ = self.sweep(&shutdown).await;
        }
    }

    async fn handle(
        &self,
        request: CallRequest,
        received: Instant,
    ) -> Result<(CallReply, call_request::Frame), Status> {
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        let frame = request.frame.ok_or_else(|| invalid("frame is required"))?;
        let kind = frame.clone();
        let reply = match frame {
            call_request::Frame::Invite(invite) => self.invite(user, device, invite).await?,
            other => {
                self.frame(user, device, other, received).await?;
                CallReply::default()
            }
        };
        Ok((reply, kind))
    }

    async fn invite(
        &self,
        caller: UserId,
        device: DeviceId,
        frame: v1::CallInvite,
    ) -> Result<CallReply, Status> {
        let invite = wire::invite(frame)?;
        let callee = invite.callee;
        if callee == caller {
            return Err(invalid("callee_user_id: a user cannot call themselves"));
        }
        // Before the permission: an agent's permission is stateful (a call grant or the
        // outgoing gate), so a resend must not be gated again.
        if self.repeated_invite(invite.call_id, caller, device).await? {
            return Ok(CallReply::default());
        }
        let by_agent = self
            .messaging
            .call_permission(caller, callee)
            .await
            .map_err(|error| match error {
                DomainError::UnknownUser => domain_error(DomainError::NotContact),
                other => domain_error(other),
            })?;
        let now = self.redis.now().await.map_err(redis_failed)?;
        let ringing = self
            .redis
            .live_devices(callee, now)
            .await
            .map_err(redis_failed)?;
        if ringing.is_empty() {
            // As the create would answer: the caller's own call first, then busy.
            if let Some(current) = self.redis.user_call(caller).await.map_err(redis_failed)? {
                if current == invite.call_id
                    && self.repeated_invite(current, caller, device).await?
                {
                    return Ok(CallReply::default());
                }
                return Err(in_call());
            }
            let busy = self
                .redis
                .user_call(callee)
                .await
                .map_err(redis_failed)?
                .is_some();
            let reason = if busy {
                EndReason::Busy
            } else {
                EndReason::Unreachable
            };
            return Ok(self.not_ringing(invite.call_id, caller, callee, reason, now, by_agent));
        }
        let (mut record, deliveries) = start(
            invite.call_id,
            caller,
            device,
            callee,
            invite.offer,
            ringing,
            now,
            &self.timing,
        );
        record.by_agent = by_agent;
        let json = encode(&record)?;
        let write = Write {
            call: record.call_id,
            expected: None,
            record: Some(&json),
            ttl_ms: ttl(&record, now),
            due_ms: record.due_ms,
            lock: vec![caller, callee],
            release: Vec::new(),
        };
        match self.redis.apply(&write).await.map_err(redis_failed)? {
            Applied::Ok => {}
            Applied::InCall => return Err(in_call()),
            Applied::Busy => {
                return Ok(self.not_ringing(
                    invite.call_id,
                    caller,
                    callee,
                    EndReason::Busy,
                    now,
                    by_agent,
                ));
            }
            Applied::Exists => {
                return if self.repeated_invite(record.call_id, caller, device).await? {
                    Ok(CallReply::default())
                } else {
                    Err(invalid("call_id is already used"))
                };
            }
            Applied::Conflict => return Err(unavailable()),
        }
        self.publish(&record, deliveries, now).await;
        tracing::info!(
            target: "security",
            event = "call.started",
            call_id = %record.call_id,
            caller = %caller.0,
            callee = %callee.0,
            "a call started ringing"
        );
        self.metrics.call_started();
        Ok(CallReply::default())
    }

    /// Whether `call` is a ringing call of `device` of `caller`: an invite sent again
    /// (its reply was lost) is accepted without effect.
    async fn repeated_invite(
        &self,
        call: Uuid,
        caller: UserId,
        device: DeviceId,
    ) -> Result<bool, Status> {
        let (stored, _) = self.redis.load(call).await.map_err(redis_failed)?;
        Ok(decode(call, stored).is_some_and(|(_, record)| {
            record.caller == caller
                && record.caller_device == device
                && record.state == CallState::Ringing
        }))
    }

    /// An invite that cannot ring: answered `CallEnded` at once, audited as an end and
    /// written to the call history (the callee's only for `Busy`). `by_agent`: the caller
    /// is an agent.
    fn not_ringing(
        &self,
        call: Uuid,
        caller: UserId,
        callee: UserId,
        reason: EndReason,
        now: i64,
        by_agent: bool,
    ) -> CallReply {
        audit_end(call, caller, callee, reason, 0, false);
        self.metrics.call_ended(reason);
        let rows = ended_rows(call, caller, callee, now, None, now, reason, by_agent);
        self.history.record_in_background(call, rows, now);
        CallReply {
            ended: Some(wire::ended(call, reason, None)),
        }
    }

    async fn frame(
        &self,
        user: UserId,
        device: DeviceId,
        frame: call_request::Frame,
        received: Instant,
    ) -> Result<(), Status> {
        let (call, event) = wire::frame_event(device, frame)?;
        for _ in 0..MAX_ATTEMPTS {
            let (stored, now) = self.redis.load(call).await.map_err(redis_failed)?;
            let Some((version, record)) = decode(call, stored) else {
                return Err(not_found());
            };
            if record.caller != user && record.callee != user {
                return Err(not_found());
            }
            let outcome = transition(&record, Some(user), event.clone(), now, &self.timing)
                .map_err(refused)?;
            if self
                .commit(&record, version, outcome, now, Some(received))
                .await?
            {
                return Ok(());
            }
        }
        Err(unavailable())
    }

    async fn attach_device(
        &self,
        user: UserId,
        device: DeviceId,
        conn: Uuid,
    ) -> Result<CallSync, Status> {
        let stored = self
            .redis
            .attach(user, device, conn)
            .await
            .map_err(redis_failed)?;
        if !stored {
            // A newer connection of the device is registered: it owns the device's calls.
            return Ok(CallSync::default());
        }
        let Some(call) = self.redis.user_call(user).await.map_err(redis_failed)? else {
            return Ok(CallSync::default());
        };
        for _ in 0..MAX_ATTEMPTS {
            let (stored, now) = self.redis.load(call).await.map_err(redis_failed)?;
            let Some((version, record)) = decode(call, stored) else {
                return Ok(CallSync::default());
            };
            let event = CallEvent::Attach { user, device };
            let mut outcome =
                transition(&record, None, event, now, &self.timing).map_err(refused)?;
            if is_party_device(&record, user, device) {
                // Always bump the version, so a tick that read the registry before this
                // attach loses its compare-and-set and reads the device as live.
                outcome.changed = true;
            }
            let next = outcome.record.clone();
            if self.commit(&record, version, outcome, now, None).await? {
                let calls = next
                    .and_then(|next| sync_view(&next, user, device, now))
                    .map(|view| wire::call_state(&view))
                    .into_iter()
                    .collect();
                return Ok(CallSync { calls });
            }
        }
        Err(unavailable())
    }

    async fn detach_device(
        &self,
        user: UserId,
        device: DeviceId,
        conn: Uuid,
    ) -> Result<(), Status> {
        let matched = self
            .redis
            .detach(user, device, conn)
            .await
            .map_err(redis_failed)?;
        if !matched {
            return Ok(());
        }
        let Some(call) = self.redis.user_call(user).await.map_err(redis_failed)? else {
            return Ok(());
        };
        for _ in 0..MAX_ATTEMPTS {
            let (stored, now) = self.redis.load(call).await.map_err(redis_failed)?;
            let Some((version, record)) = decode(call, stored) else {
                return Ok(());
            };
            let event = CallEvent::Detach { user, device };
            let outcome = transition(&record, None, event, now, &self.timing).map_err(refused)?;
            if self.commit(&record, version, outcome, now, None).await? {
                return Ok(());
            }
        }
        Err(unavailable())
    }

    /// Ticks one claimed call.
    async fn tick(&self, call: Uuid) -> Result<(), Status> {
        for _ in 0..MAX_ATTEMPTS {
            let (stored, now) = self.redis.load(call).await.map_err(redis_failed)?;
            let Some((version, record)) = decode(call, stored) else {
                // The record expired or ended without its due entry: nothing to tick.
                return self.redis.zrem_due(call).await.map_err(redis_failed);
            };
            if record.due_ms > now {
                // Rescheduled by a write after the claim, which also set its due entry.
                return Ok(());
            }
            let (gone, live) = if record.state == CallState::Active {
                self.liveness(&record, now).await?
            } else {
                (Vec::new(), Vec::new())
            };
            let event = CallEvent::Tick { gone, live };
            let outcome = transition(&record, None, event, now, &self.timing).map_err(refused)?;
            if self.commit(&record, version, outcome, now, None).await? {
                return Ok(());
            }
        }
        Err(unavailable())
    }

    /// The sides of an active call whose party device has no registered connection, or
    /// one without a live presence lease (gone), and the other sides (live).
    async fn liveness(
        &self,
        record: &CallRecord,
        now: i64,
    ) -> Result<(Vec<Side>, Vec<Side>), Status> {
        let parties: Vec<(Side, UserId, DeviceId)> = [
            (Side::Caller, record.caller, Some(record.caller_device)),
            (Side::Callee, record.callee, record.callee_device),
        ]
        .into_iter()
        .filter_map(|(side, user, device)| device.map(|device| (side, user, device)))
        .collect();
        let devices: Vec<(UserId, DeviceId)> = parties
            .iter()
            .map(|&(_, user, device)| (user, device))
            .collect();
        let live = self
            .redis
            .devices_live(&devices, now)
            .await
            .map_err(redis_failed)?;
        let mut gone = Vec::new();
        let mut connected = Vec::new();
        for ((side, _, _), live) in parties.into_iter().zip(live) {
            if live {
                connected.push(side);
            } else {
                gone.push(side);
            }
        }
        Ok((gone, connected))
    }

    /// Writes `outcome` over `before`, read at `version`, then publishes, audits and
    /// counts it. `Ok(false)` when another write came first: read again.
    async fn commit(
        &self,
        before: &CallRecord,
        version: u64,
        outcome: Outcome,
        now: i64,
        received: Option<Instant>,
    ) -> Result<bool, Status> {
        if !outcome.changed {
            // Nothing to write; a repeated accept still re-sends its frame.
            if !outcome.deliveries.is_empty() {
                self.publish(before, outcome.deliveries, now).await;
            }
            return Ok(true);
        }
        let json;
        let write = match &outcome.record {
            Some(next) => {
                json = encode(next)?;
                Write {
                    call: before.call_id,
                    expected: Some(version),
                    record: Some(&json),
                    ttl_ms: ttl(next, now),
                    due_ms: next.due_ms,
                    lock: vec![before.caller, before.callee],
                    release: Vec::new(),
                }
            }
            None => Write {
                call: before.call_id,
                expected: Some(version),
                record: None,
                ttl_ms: 0,
                due_ms: 0,
                lock: Vec::new(),
                release: vec![before.caller, before.callee],
            },
        };
        match self.redis.apply(&write).await.map_err(redis_failed)? {
            Applied::Ok => {}
            Applied::Conflict => return Ok(false),
            Applied::InCall | Applied::Busy | Applied::Exists => return Err(unavailable()),
        }
        self.publish(before, outcome.deliveries, now).await;
        if outcome.answered {
            self.metrics
                .call_answered(received.map(|at| at.elapsed()).unwrap_or_default());
        }
        if let Some(reason) = outcome.ended {
            let duration_s = before
                .answered_ms
                .map_or(0, |answered| (now - answered).max(0) / 1000);
            audit_end(
                before.call_id,
                before.caller,
                before.callee,
                reason,
                duration_s,
                before.answered_ms.is_some(),
            );
            self.metrics.call_ended(reason);
            let rows = ended_rows(
                before.call_id,
                before.caller,
                before.callee,
                before.created_ms,
                before.answered_ms,
                now,
                reason,
                before.by_agent,
            );
            self.history.record_in_background(before.call_id, rows, now);
        }
        Ok(true)
    }

    /// Publishes the frames of `deliveries` in one pipeline. A failure is logged: the
    /// devices recover through `CallSync` and their own timers.
    async fn publish(&self, record: &CallRecord, deliveries: Vec<Delivery>, now: i64) {
        let messages: Vec<(String, Vec<u8>)> = deliveries
            .into_iter()
            .map(|delivery| wire::signal(record, delivery, now))
            .collect();
        if let Err(error) = self.notifier.publish_raw(&messages).await {
            tracing::warn!(
                %error,
                count = messages.len(),
                "could not publish call frames; devices recover through CallSync"
            );
        }
    }
}

#[async_trait]
impl<S, C, N> CallsApi for CallService<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn call(&self, request: CallRequest) -> Result<CallReply, Status> {
        let received = Instant::now();
        match self.handle(request, received).await {
            Ok((reply, frame)) => {
                self.metrics
                    .frame_handled(wire::signal_frame(&frame), received.elapsed());
                Ok(reply)
            }
            Err(status) => {
                self.metrics
                    .frame_refused(push_proto::error_from_status(&status).code());
                Err(status)
            }
        }
    }

    async fn attach(&self, request: CallDeviceRequest) -> Result<CallSync, Status> {
        let (user, device, conn) = device_request(&request)?;
        self.attach_device(user, device, conn).await
    }

    async fn detach(&self, request: CallDeviceRequest) -> Result<(), Status> {
        let (user, device, conn) = device_request(&request)?;
        self.detach_device(user, device, conn).await
    }
}

/// Whether `device` of `user` is the caller's device, or the answering device of an active
/// call.
fn is_party_device(record: &CallRecord, user: UserId, device: DeviceId) -> bool {
    (user == record.caller && device == record.caller_device)
        || (record.state == CallState::Active
            && user == record.callee
            && record.callee_device == Some(device))
}

fn device_request(request: &CallDeviceRequest) -> Result<(UserId, DeviceId, Uuid), Status> {
    Ok((
        parse_id(&request.user_id, "user_id", UserId)?,
        parse_id(&request.device_id, "device_id", DeviceId)?,
        parse_id(
            &request.connection_id,
            "connection_id",
            std::convert::identity,
        )?,
    ))
}

fn audit_end(
    call: Uuid,
    caller: UserId,
    callee: UserId,
    reason: EndReason,
    duration_s: i64,
    answered: bool,
) {
    tracing::info!(
        target: "security",
        event = "call.ended",
        call_id = %call,
        caller = %caller.0,
        callee = %callee.0,
        reason = reason.as_str(),
        duration_s,
        answered,
        "a call ended"
    );
}

/// How long a record and its locks live from `now` (spec §5).
fn ttl(record: &CallRecord, now: i64) -> i64 {
    match record.state {
        CallState::Ringing => record.ring_deadline_ms + RINGING_EXTRA_TTL_MS - now,
        CallState::Active => ACTIVE_TTL_MS,
    }
}

fn encode(record: &CallRecord) -> Result<String, Status> {
    serde_json::to_string(record).map_err(|error| {
        tracing::warn!(
            category = category(&error),
            "a call record could not be encoded"
        );
        unavailable()
    })
}

/// The stored record, or `None` when there is none or it cannot be read (logged; the
/// call is then treated as gone and expires by its TTL).
fn decode(call: Uuid, stored: Option<(u64, String)>) -> Option<(u64, CallRecord)> {
    let (version, json) = stored?;
    match serde_json::from_str(&json) {
        Ok(record) => Some((version, record)),
        Err(error) => {
            tracing::warn!(
                call_id = %call,
                category = category(&error),
                "a call record could not be read"
            );
            None
        }
    }
}

/// A JSON error's category: its text could quote the record.
fn category(error: &serde_json::Error) -> &'static str {
    match error.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    }
}

fn call_error(code: Code, error_code: ErrorCode, message: &str) -> Status {
    error_status(
        code,
        v1::Error {
            code: error_code.into(),
            message: message.to_owned(),
            retryable: error_code == ErrorCode::Unavailable,
            ..v1::Error::default()
        },
    )
}

fn not_found() -> Status {
    call_error(
        Code::NotFound,
        ErrorCode::CallNotFound,
        "no such call for this device",
    )
}

fn in_call() -> Status {
    call_error(
        Code::FailedPrecondition,
        ErrorCode::InCall,
        "the caller is already in a call",
    )
}

/// Redis failed, or every attempt lost a compare-and-set race: retryable.
fn unavailable() -> Status {
    call_error(
        Code::Unavailable,
        ErrorCode::Unavailable,
        "calls are temporarily unavailable; retry",
    )
}

fn refused(refusal: Refusal) -> Status {
    match refusal {
        Refusal::NotFound => not_found(),
        Refusal::AlreadyAnswered => call_error(
            Code::FailedPrecondition,
            ErrorCode::AlreadyAnswered,
            "the call was already answered",
        ),
        Refusal::Limit => call_error(
            Code::ResourceExhausted,
            ErrorCode::CallLimit,
            "the call's frame limit is reached",
        ),
    }
}

/// A Redis failure, logged with the error only.
fn redis_failed(error: ::redis::RedisError) -> Status {
    tracing::warn!(%error, "the call state could not be read or written");
    unavailable()
}
