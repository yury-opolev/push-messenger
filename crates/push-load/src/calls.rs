//! A calls run: concurrent 1:1 voice calls between prepared sample pairs, signalling only.
//! It refreshes the pairs' tokens once (as a run does), connects both devices of each
//! pair advertising `CAPABILITY_CALLS`, keeps them alive with Pings, and has each pair
//! place one call after another: the caller sends `CallInvite`, the callee answers its
//! `CallIncoming` at once, both trickle a candidate, the call is held, the caller hangs
//! up and both await `CallEnded`. No media flows and the SDP is synthetic. Connections
//! are never reconnected: a dropped one fails the run.

pub mod report;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    CallAccept, CallEndReason, CallHangup, CallIce, CallInvite, CallMedia, CallSync, Capability,
    ErrorCode, Ping,
};
use push_proto::{decode_frame, id_bytes};
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::commandstats::RedisDeltas;
use crate::config::{CALL_MARGIN, CALL_PAUSE, CallsOptions, Target};
use crate::metrics::{self, CloseCodes, Counters, LAG_LIMIT, Samples, bump, read};
use crate::report::{Connections, Generator, Status};
use crate::run::{
    FIRST_BACKOFF, MAX_BACKOFF, PONG_TIMEOUT, READY_DEADLINE, STOP_GRACE, jitter, refresh_all,
    stop_for, watch_interrupt,
};
use crate::socket::{self, ABNORMAL_CLOSE, Opened, Ready, Socket};
use crate::state::{DeviceRef, State};
use report::{CALL_PERCENTILES, CallCounts, CallLatency, CallsReport, CallsWorkload, SetupSummary};

/// A call must be accepted within this long of its invite.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Both parties must hear `CallEnded` within this long of the hangup.
pub const ENDED_TIMEOUT: Duration = Duration::from_secs(10);
/// The first call waits at most this long for both `CallSync`s and leftover calls to end.
const SYNC_WAIT: Duration = Duration::from_secs(10);
/// How long the end of the run waits for the calls still live to end.
const HANGUP_GRACE: Duration = Duration::from_secs(5);

/// A synthetic audio-only offer and answer: valid SDP, never used for media.
const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=push-load\r\nt=0 0\r\n\
    m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:111 opus/48000/2\r\n";
const ANSWER: &str = "v=0\r\no=- 2 1 IN IP4 0.0.0.0\r\ns=push-load\r\nt=0 0\r\n\
    m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:111 opus/48000/2\r\n";
/// A host candidate on a documentation address (RFC 5737): never reachable.
const CANDIDATE: &str = "candidate:1 1 udp 2122260223 192.0.2.1 54321 typ host";

/// A count that goes up and down, and its highest value.
#[derive(Debug, Default)]
pub struct Gauge {
    now: AtomicI64,
    peak: AtomicI64,
}

impl Gauge {
    pub fn up(&self) {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }

    pub fn down(&self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    pub fn peak(&self) -> i64 {
        self.peak.load(Ordering::SeqCst)
    }
}

/// Names and how often each came.
#[derive(Debug, Default)]
pub struct Tally(Mutex<BTreeMap<String, u64>>);

impl Tally {
    pub fn add(&self, name: String) {
        *self.0.lock().expect("tally lock").entry(name).or_default() += 1;
    }

    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.0.lock().expect("tally lock").clone()
    }
}

/// What every pair of a calls run adds to.
#[derive(Debug, Default)]
pub struct CallStats {
    pub placed: AtomicU64,
    pub accepted: AtomicU64,
    pub completed: AtomicU64,
    pub unanswered: AtomicU64,
    pub endings_missing: AtomicU64,
    pub unexpected_frames: AtomicU64,
    pub ice_sent: AtomicU64,
    pub ice_received: AtomicU64,
    pub call_syncs: AtomicU64,
    pub stale_calls: AtomicU64,
    pub not_offered: AtomicU64,
    /// `CallEnded` reasons the callers heard.
    pub ended: Tally,
    /// `CallEnded` reasons the callees heard.
    pub callee_ended: Tally,
    pub errors: Tally,
    /// Calls ringing or active.
    pub live: Gauge,
    pub active: Gauge,
    pub setup: Samples,
}

impl CallStats {
    fn counts(&self) -> CallCounts {
        CallCounts {
            placed: read(&self.placed),
            accepted: read(&self.accepted),
            completed: read(&self.completed),
            unanswered: read(&self.unanswered),
            endings_missing: read(&self.endings_missing),
            ended: self.ended.snapshot(),
            callee_ended: self.callee_ended.snapshot(),
            errors: self.errors.snapshot(),
            unexpected_frames: read(&self.unexpected_frames),
            ice_sent: read(&self.ice_sent),
            ice_received: read(&self.ice_received),
            call_syncs: read(&self.call_syncs),
            stale_calls: read(&self.stale_calls),
            not_offered: read(&self.not_offered),
            peak_active: self.active.peak(),
            peak_ringing_or_active: self.live.peak(),
        }
    }
}

/// `CALL_END_REASON_BUSY` as `busy`; an unknown value as its number.
fn reason_name(reason: i32) -> String {
    CallEndReason::try_from(reason).map_or_else(
        |_| reason.to_string(),
        |reason| {
            reason
                .as_str_name()
                .trim_start_matches("CALL_END_REASON_")
                .to_ascii_lowercase()
        },
    )
}

/// `ERROR_CODE_RATE_LIMITED` as `rate_limited`; an unknown value as its number.
fn code_name(code: i32) -> String {
    ErrorCode::try_from(code).map_or_else(
        |_| code.to_string(),
        |code| {
            code.as_str_name()
                .trim_start_matches("ERROR_CODE_")
                .to_ascii_lowercase()
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Caller,
    Callee,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Self::Caller => 0,
            Self::Callee => 1,
        }
    }
}

fn hangup(call_id: &[u8]) -> Body {
    Body::CallHangup(CallHangup {
        call_id: call_id.to_vec(),
        reason: None,
    })
}

/// One candidate and the end of candidates: two `CallIce` frames.
fn trickle(side: Side, call_id: &[u8]) -> [(Side, Body); 2] {
    let ice = |candidate: &str, end_of_candidates| {
        Body::CallIce(CallIce {
            call_id: call_id.to_vec(),
            candidate: candidate.to_owned(),
            sdp_mid: Some("0".to_owned()),
            sdp_mline_index: Some(0),
            end_of_candidates,
            username_fragment: None,
        })
    };
    [(side, ice(CANDIDATE, false)), (side, ice("", true))]
}

#[derive(Debug)]
struct Call {
    id: Vec<u8>,
    invited_at: Instant,
    rang: bool,
    accepted: bool,
    /// Ringing: when it must have been accepted. Active: when the caller hangs up.
    due: Instant,
    hangup_sent: Option<Instant>,
    caller_ended: bool,
    callee_ended: bool,
}

#[derive(Debug)]
enum Phase {
    /// The next call may be placed from then on.
    Waiting(Instant),
    Live(Call),
    /// No more calls for this pair.
    Quiet,
}

/// The calls of one pair: what to send for each frame received and each timer, apart
/// from the sockets so that it can be tested without a target.
#[derive(Debug)]
pub struct PairCalls<'a> {
    stats: &'a CallStats,
    caller_user: Vec<u8>,
    callee_user: Vec<u8>,
    hold: Duration,
    period: Duration,
    /// No invite is sent after this.
    last_invite: Instant,
    phase: Phase,
    synced: [bool; 2],
    sync_by: Instant,
    /// Leftover calls each side's `CallSync` listed, until that side hears `CallEnded`.
    stale: HashSet<(Side, Vec<u8>)>,
    /// Every leftover call listed: a refused hangup of one is no error of the run.
    leftovers: HashSet<Vec<u8>>,
    /// Leftover calls a device of the pair hung up: each is hung up once.
    hung_up: HashSet<Vec<u8>>,
    /// The callee's leftover calls with the caller, until the caller's `CallSync`: the
    /// caller hangs up those its device holds, the callee the rest.
    deferred: HashSet<Vec<u8>>,
    /// A `CallError` came: the pair places no further call.
    refused: bool,
    stopping: bool,
}

impl<'a> PairCalls<'a> {
    pub fn new(
        stats: &'a CallStats,
        caller_user: Uuid,
        callee_user: Uuid,
        options: &CallsOptions,
        last_invite: Instant,
        now: Instant,
    ) -> Self {
        Self {
            stats,
            caller_user: id_bytes(caller_user),
            callee_user: id_bytes(callee_user),
            hold: options.hold,
            period: options.period(),
            last_invite,
            phase: Phase::Waiting(now),
            synced: [false; 2],
            sync_by: now + SYNC_WAIT,
            stale: HashSet::new(),
            leftovers: HashSet::new(),
            hung_up: HashSet::new(),
            deferred: HashSet::new(),
            refused: false,
            stopping: false,
        }
    }

    /// Whether no call is ringing or active.
    pub fn idle(&self) -> bool {
        !matches!(self.phase, Phase::Live(_))
    }

    /// When [`Self::on_timer`] has something to do.
    pub fn wake_at(&self) -> Option<Instant> {
        match &self.phase {
            Phase::Waiting(_) if self.stopping => None,
            Phase::Waiting(at) => {
                let settled = self.synced == [true, true] && self.stale.is_empty();
                Some(if settled {
                    *at
                } else {
                    (*at).max(self.sync_by)
                })
            }
            Phase::Live(call) => Some(call.hangup_sent.map_or(call.due, |at| at + ENDED_TIMEOUT)),
            Phase::Quiet => None,
        }
    }

    /// Places the next call, hangs up, or gives up on a call, as its time has come.
    pub fn on_timer(&mut self, now: Instant) -> Vec<(Side, Body)> {
        if self.wake_at().is_none_or(|at| now < at) {
            return Vec::new();
        }
        match &mut self.phase {
            Phase::Waiting(_) => {
                if self.refused || now > self.last_invite {
                    self.phase = Phase::Quiet;
                    return Vec::new();
                }
                let id = id_bytes(Uuid::now_v7());
                bump(&self.stats.placed);
                self.stats.live.up();
                self.phase = Phase::Live(Call {
                    id: id.clone(),
                    invited_at: now,
                    rang: false,
                    accepted: false,
                    due: now + SETUP_TIMEOUT,
                    hangup_sent: None,
                    caller_ended: false,
                    callee_ended: false,
                });
                vec![(
                    Side::Caller,
                    Body::CallInvite(CallInvite {
                        call_id: id,
                        callee_user_id: self.callee_user.clone(),
                        offer_sdp: OFFER.to_owned(),
                        media: vec![i32::from(CallMedia::Audio)],
                    }),
                )]
            }
            Phase::Live(call) => {
                if call.hangup_sent.is_some() {
                    self.abandon();
                    return Vec::new();
                }
                if !call.accepted {
                    bump(&self.stats.unanswered);
                }
                call.hangup_sent = Some(now);
                vec![(Side::Caller, hangup(&call.id))]
            }
            Phase::Quiet => Vec::new(),
        }
    }

    /// The run ends: no further call, and the live one is hung up.
    pub fn stop(&mut self, now: Instant) -> Vec<(Side, Body)> {
        self.stopping = true;
        match &mut self.phase {
            Phase::Live(call) if call.hangup_sent.is_none() => {
                call.hangup_sent = Some(now);
                vec![(Side::Caller, hangup(&call.id))]
            }
            Phase::Live(_) => Vec::new(),
            _ => {
                self.phase = Phase::Quiet;
                Vec::new()
            }
        }
    }

    /// The stop grace ran out: a call still live never ended.
    pub fn out_of_time(&mut self) {
        if !self.idle() {
            self.abandon();
        }
    }

    /// Gives up on the live call, as a party heard no `CallEnded`.
    fn abandon(&mut self) {
        if let Phase::Live(call) = &self.phase {
            bump(&self.stats.endings_missing);
            if !call.caller_ended {
                self.stats.live.down();
                if call.accepted {
                    self.stats.active.down();
                }
            }
        }
        self.phase = Phase::Quiet;
    }

    /// A hangup of a leftover call from `side`, unless the pair hung it up already.
    fn hang_up_leftover(&mut self, side: Side, call_id: &[u8]) -> Option<(Side, Body)> {
        self.hung_up
            .insert(call_id.to_vec())
            .then(|| (side, hangup(call_id)))
    }

    /// A side's `CallSync`: each leftover call is hung up once, by the caller when both
    /// devices of the pair hold it, else by the device that holds it.
    fn on_sync(&mut self, side: Side, sync: CallSync) -> Vec<(Side, Body)> {
        bump(&self.stats.call_syncs);
        self.synced[side.index()] = true;
        let mut out = Vec::new();
        for call in sync.calls {
            let id = call.call_id;
            if self.leftovers.insert(id.clone()) {
                bump(&self.stats.stale_calls);
            }
            self.stale.insert((side, id.clone()));
            match side {
                Side::Caller => {
                    let owed_by_callee = self.deferred.remove(&id);
                    if call.this_device {
                        out.extend(self.hang_up_leftover(Side::Caller, &id));
                    } else if owed_by_callee {
                        out.extend(self.hang_up_leftover(Side::Callee, &id));
                    }
                }
                Side::Callee if call.this_device => {
                    if call.peer_user_id == self.caller_user && !self.synced[Side::Caller.index()] {
                        self.deferred.insert(id);
                    } else {
                        out.extend(self.hang_up_leftover(Side::Callee, &id));
                    }
                }
                Side::Callee => {}
            }
        }
        if side == Side::Caller {
            // The caller's device does not hold these: the callee hangs them up.
            for id in std::mem::take(&mut self.deferred) {
                out.extend(self.hang_up_leftover(Side::Callee, &id));
            }
        }
        out
    }

    fn unexpected(&self) -> Vec<(Side, Body)> {
        bump(&self.stats.unexpected_frames);
        Vec::new()
    }

    /// A call frame from the gateway; other frames are ignored.
    pub fn on_frame(&mut self, side: Side, body: Body, now: Instant) -> Vec<(Side, Body)> {
        match body {
            Body::CallSync(sync) => self.on_sync(side, sync),
            Body::CallIncoming(incoming) => match &mut self.phase {
                Phase::Live(call)
                    if side == Side::Callee && incoming.call_id == call.id && !call.rang =>
                {
                    call.rang = true;
                    let mut out = vec![(
                        Side::Callee,
                        Body::CallAccept(CallAccept {
                            call_id: call.id.clone(),
                            answer_sdp: ANSWER.to_owned(),
                        }),
                    )];
                    out.extend(trickle(Side::Callee, &call.id));
                    self.stats.ice_sent.fetch_add(2, Ordering::Relaxed);
                    out
                }
                _ => self.unexpected(),
            },
            Body::CallAccepted(accepted) => match &mut self.phase {
                Phase::Live(call)
                    if side == Side::Caller
                        && accepted.call_id == call.id
                        && !call.accepted
                        && call.hangup_sent.is_none() =>
                {
                    call.accepted = true;
                    self.stats.setup.record(now - call.invited_at);
                    bump(&self.stats.accepted);
                    self.stats.active.up();
                    call.due = now + self.hold;
                    self.stats.ice_sent.fetch_add(2, Ordering::Relaxed);
                    trickle(Side::Caller, &call.id).into()
                }
                _ => self.unexpected(),
            },
            Body::CallEnded(ended) => {
                if self.stale.remove(&(side, ended.call_id.clone())) {
                    return Vec::new();
                }
                let Phase::Live(call) = &mut self.phase else {
                    return self.unexpected();
                };
                if ended.call_id != call.id {
                    return self.unexpected();
                }
                match side {
                    Side::Caller if !call.caller_ended => {
                        call.caller_ended = true;
                        self.stats.ended.add(reason_name(ended.reason));
                        self.stats.live.down();
                        if call.accepted {
                            self.stats.active.down();
                            if ended.reason() == CallEndReason::Completed {
                                bump(&self.stats.completed);
                            }
                        }
                    }
                    Side::Callee if !call.callee_ended => {
                        call.callee_ended = true;
                        self.stats.callee_ended.add(reason_name(ended.reason));
                    }
                    _ => return self.unexpected(),
                }
                if call.caller_ended && (call.callee_ended || !call.rang) {
                    let next = (now + CALL_PAUSE).max(call.invited_at + self.period);
                    self.phase = if self.stopping || self.refused {
                        Phase::Quiet
                    } else {
                        Phase::Waiting(next)
                    };
                }
                Vec::new()
            }
            Body::CallError(error) => {
                if self.leftovers.contains(&error.call_id) {
                    return Vec::new();
                }
                self.stats.errors.add(code_name(error.code));
                self.refused = true;
                if let Phase::Live(call) = &self.phase {
                    // Refused before any accept or hangup: the invite itself, so there
                    // is no call.
                    if side == Side::Caller
                        && error.call_id == call.id
                        && !call.accepted
                        && call.hangup_sent.is_none()
                    {
                        self.stats.live.down();
                        self.phase = Phase::Quiet;
                    }
                }
                Vec::new()
            }
            Body::PeerCallIce(_) => {
                bump(&self.stats.ice_received);
                Vec::new()
            }
            Body::CallTaken(_) | Body::PeerCallRenegotiate(_) => self.unexpected(),
            _ => Vec::new(),
        }
    }
}

struct Shared {
    target: Target,
    options: CallsOptions,
    /// The run is over: hang up and close.
    cancel: CancellationToken,
    /// The target offers no calls: end the run early.
    abort: CancellationToken,
    last_invite: Instant,
    heartbeat_ms: AtomicU64,
    counters: Counters,
    closes: CloseCodes,
    refusals: CloseCodes,
    connect: Samples,
    ping_rtt: Samples,
    calls: CallStats,
}

pub async fn run(
    target: &Target,
    state_path: &Path,
    options: &CallsOptions,
) -> anyhow::Result<CallsReport> {
    options.validate()?;
    crate::state::ensure_outside_git(state_path)?;
    let mut state =
        State::load(state_path)?.context("there is no state file: run prepare first")?;
    if state.target != target.authority() {
        bail!(
            "{} was prepared for {}, not {}",
            state_path.display(),
            state.target,
            target.authority()
        );
    }
    let pairs: Vec<(DeviceRef, DeviceRef)> = state
        .pairs
        .iter()
        .filter(|pair| state.pair_is_ready(pair))
        .map(|pair| (pair.sender, pair.receiver))
        .collect();
    if pairs.len() < options.calls {
        bail!(
            "only {} prepared sample pairs are ready; --calls {} needs more (run prepare with --sample-pairs {} and twice as many accounts)",
            pairs.len(),
            options.calls,
            options.calls
        );
    }
    let pairs = &pairs[..options.calls];
    // A Redis that cannot be read stops the run before any token is refreshed.
    if let Some(redis) = &options.redis {
        if let Err(error) = redis.read_commandstats().await {
            bail!("the Redis of --redis-url-file could not be read: {error}");
        }
    }
    let selected: Vec<DeviceRef> = pairs
        .iter()
        .flat_map(|(caller, callee)| [*caller, *callee])
        .collect();
    let started_unix_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let started = Instant::now();

    let interrupt = CancellationToken::new();
    tokio::spawn(watch_interrupt(interrupt.clone()));
    let counters = Counters::default();
    let tokens = refresh_all(
        target, &mut state, state_path, &selected, &counters, &interrupt,
    )
    .await?;

    let redis_before = match &options.redis {
        Some(redis) => Some(redis.read_commandstats().await.map_err(|error| {
            anyhow::anyhow!("the Redis of --redis-url-file could not be read: {error}")
        })?),
        None => None,
    };

    let lag = Arc::new(Samples::default());
    let lag_cancel = CancellationToken::new();
    let lag_watch = tokio::spawn(metrics::watch_lag(lag.clone(), lag_cancel.clone()));

    let ramp_start = Instant::now();
    let shared = Arc::new(Shared {
        target: target.clone(),
        options: options.clone(),
        cancel: CancellationToken::new(),
        abort: CancellationToken::new(),
        last_invite: ramp_start + options.ramp_time() + options.duration
            - options.hold
            - CALL_MARGIN,
        heartbeat_ms: AtomicU64::new(0),
        counters,
        closes: CloseCodes::default(),
        refusals: CloseCodes::default(),
        connect: Samples::default(),
        ping_rtt: Samples::default(),
        calls: CallStats::default(),
    });
    let mut tasks = JoinSet::new();
    let interval = Duration::from_secs_f64(2.0 / f64::from(options.ramp_per_second));
    let mut tokens = tokens.into_iter();
    for index in 0..pairs.len() {
        let (caller_token, callee_token) = (tokens.next().flatten(), tokens.next().flatten());
        let due = ramp_start + interval.mul_f64(index as f64);
        tokio::select! {
            () = interrupt.cancelled() => break,
            () = shared.abort.cancelled() => break,
            () = tokio::time::sleep_until(due) => {}
        }
        // A device whose refresh failed is counted already; its pair places no call.
        let (Some(caller_token), Some(callee_token)) = (caller_token, callee_token) else {
            continue;
        };
        tasks.spawn(pair(shared.clone(), caller_token, callee_token));
    }
    let ramp_end = Instant::now();

    let mut min_active: Option<i64> = None;
    if !interrupt.is_cancelled() && !shared.abort.is_cancelled() {
        let run_end = ramp_end + options.duration;
        let mut sampler = tokio::time::interval(Duration::from_secs(1));
        sampler.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = interrupt.cancelled() => break,
                () = shared.abort.cancelled() => break,
                () = tokio::time::sleep_until(run_end) => break,
                _ = sampler.tick() => {
                    let active = shared.counters.active();
                    min_active = Some(min_active.map_or(active, |least| least.min(active)));
                }
            }
        }
    }
    shared.cancel.cancel();
    let stop_deadline = Instant::now() + STOP_GRACE;
    loop {
        match tokio::time::timeout_at(stop_deadline, tasks.join_next()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                tasks.abort_all();
                break;
            }
        }
    }
    lag_cancel.cancel();
    let _ = lag_watch.await;
    let redis = match (&options.redis, &redis_before) {
        (Some(redis), Some(before)) => Some(match redis.read_commandstats().await {
            Ok(after) => RedisDeltas::between(before, &after, read(&shared.calls.placed)),
            Err(error) => RedisDeltas::failed(error),
        }),
        _ => None,
    };

    let c = &shared.counters;
    let lag = lag.summary();
    let heartbeat_ms = shared.heartbeat_ms.load(Ordering::SeqCst);
    let mut report = CallsReport {
        tool: "push-load".to_owned(),
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        scenario: "calls".to_owned(),
        status: Status::Failed,
        failures: Vec::new(),
        target: target.base_url(),
        target_build: options.target_build.trim().to_owned(),
        started_unix_s,
        elapsed_s: started.elapsed().as_secs_f64(),
        workload: CallsWorkload {
            calls: options.calls,
            connections: options.connections(),
            ramp_per_second: options.ramp_per_second,
            ramp_s: (ramp_end - ramp_start).as_secs_f64(),
            duration_s: options.duration.as_secs(),
            hold_s: options.hold.as_secs(),
            attempts_per_hour: options.attempts_per_hour,
            attempts_per_pair_max: options.attempts_per_pair(),
            setup_p95_limit_ms: u64::try_from(options.setup_p95.as_millis()).unwrap_or(u64::MAX),
            heartbeat_ms: (heartbeat_ms > 0).then_some(heartbeat_ms),
        },
        connections: Connections {
            ready_once: read(&c.ready_once),
            attempt_errors: read(&c.attempt_errors),
            connect_failures: read(&c.connect_failures),
            reconnect_failures: read(&c.reconnect_failures),
            auth_failures: read(&c.auth_failures),
            takeovers: read(&c.takeovers),
            rate_limited: read(&c.rate_limited),
            pong_timeouts: read(&c.pong_timeouts),
            protocol_failures: read(&c.protocol_failures),
            disconnects: read(&c.disconnects),
            reconnects: read(&c.reconnects),
            pings: read(&c.pings),
            pongs: read(&c.pongs),
            auth_expiring: read(&c.auth_expiring),
            error_frames: read(&c.error_frames),
            peak_active: c.peak(),
            min_active_after_ramp: min_active,
        },
        close_codes: shared.closes.snapshot(),
        ready_refusals: shared.refusals.snapshot(),
        calls: shared.calls.counts(),
        latency: CallLatency {
            setup: SetupSummary::of(&shared.calls.setup.sorted()),
            connect: shared.connect.summary(),
            ping_rtt: shared.ping_rtt.summary(),
        },
        generator: Generator {
            saturated: lag
                .p99_ms
                .is_some_and(|p99| u128::from(p99) > LAG_LIMIT.as_millis()),
            scheduling_lag: lag,
            lag_limit_ms: u64::try_from(LAG_LIMIT.as_millis()).unwrap_or(u64::MAX),
        },
        redis,
        percentiles: CALL_PERCENTILES.to_owned(),
    };
    let (status, failures) = report::judge(&report, interrupt.is_cancelled());
    report.status = status;
    report.failures = failures;
    Ok(report)
}

/// Connects one device, retrying (4006 while a device is relayed, a failed attempt) for
/// up to a minute. `None` when it did not become calls-active; it is counted.
async fn connect(shared: &Shared, access_token: &str) -> Option<(Socket, Ready)> {
    let first_attempt = Instant::now();
    let deadline = first_attempt + READY_DEADLINE;
    let mut backoff = FIRST_BACKOFF;
    loop {
        let opened = tokio::select! {
            () = shared.cancel.cancelled() => return None,
            opened = socket::open(&shared.target, access_token, &[Capability::Calls]) => opened,
        };
        match opened {
            Ok(Opened::Ready(socket, ready)) => {
                shared.connect.record(first_attempt.elapsed());
                bump(&shared.counters.ready_once);
                shared.counters.opened();
                let heartbeat = u64::try_from(ready.heartbeat.as_millis()).unwrap_or(u64::MAX);
                shared.heartbeat_ms.store(heartbeat, Ordering::SeqCst);
                if !ready.calls {
                    bump(&shared.calls.not_offered);
                    shared.abort.cancel();
                    socket::close(*socket).await;
                    shared.counters.closed();
                    return None;
                }
                return Some((*socket, ready));
            }
            Ok(Opened::Closed(code)) => {
                shared.refusals.record(code);
                if stop_for(&shared.counters, code) {
                    return None;
                }
            }
            Err(_) => bump(&shared.counters.attempt_errors),
        }
        if Instant::now() >= deadline {
            bump(&shared.counters.connect_failures);
            return None;
        }
        tokio::select! {
            () = shared.cancel.cancelled() => return None,
            () = tokio::time::sleep(jitter(backoff)) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// How a pair's connection ended before the run did.
enum Lost {
    Closed(u16),
    PongTimeout,
    Undecodable,
}

/// One device's connection of a pair: Pings every heartbeat.
struct Link {
    socket: Socket,
    pings: tokio::time::Interval,
    ping_sent: Option<Instant>,
}

impl Link {
    fn new(socket: Socket, ready: Ready) -> Self {
        let mut pings = tokio::time::interval_at(Instant::now() + ready.heartbeat, ready.heartbeat);
        pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            socket,
            pings,
            ping_sent: None,
        }
    }

    fn pong_due(&self) -> Instant {
        self.ping_sent.map_or_else(
            || Instant::now() + Duration::from_secs(86_400),
            |sent| sent + PONG_TIMEOUT,
        )
    }
}

/// What a message from the gateway means for the pair: a frame for [`PairCalls`], or a
/// lost connection.
fn receive(
    shared: &Shared,
    link: &mut Link,
    message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
) -> Result<Option<Body>, Lost> {
    match message {
        Some(Ok(Message::Binary(bytes))) => {
            let Ok(body) = decode_frame(&bytes) else {
                return Err(Lost::Undecodable);
            };
            match body {
                Body::Pong(_) => {
                    if let Some(sent) = link.ping_sent.take() {
                        shared.ping_rtt.record(sent.elapsed());
                        bump(&shared.counters.pongs);
                    }
                    Ok(None)
                }
                Body::AuthExpiring(_) => {
                    bump(&shared.counters.auth_expiring);
                    Ok(None)
                }
                Body::Error(error) => {
                    if error.code() == ErrorCode::RateLimited {
                        bump(&shared.counters.rate_limited);
                    } else {
                        bump(&shared.counters.error_frames);
                    }
                    Ok(None)
                }
                body => Ok(Some(body)),
            }
        }
        Some(Ok(Message::Close(frame))) => Err(Lost::Closed(socket::close_code(frame))),
        Some(Ok(_)) => Ok(None),
        Some(Err(_)) | None => Err(Lost::Closed(ABNORMAL_CLOSE)),
    }
}

fn ping(shared: &Shared, link: &mut Link, side: Side) -> Vec<(Side, Body)> {
    if link.ping_sent.is_some() {
        return Vec::new();
    }
    link.ping_sent = Some(Instant::now());
    bump(&shared.counters.pings);
    vec![(side, Body::Ping(Ping {}))]
}

/// One pair for the whole run: connect both devices, place calls until the run ends,
/// hang up what is live, close.
async fn pair(shared: Arc<Shared>, caller_token: String, callee_token: String) {
    let (caller, callee) = match tokio::join!(
        connect(&shared, &caller_token),
        connect(&shared, &callee_token)
    ) {
        (Some(caller), Some(callee)) => (caller, callee),
        (caller, callee) => {
            for (socket, _) in [caller, callee].into_iter().flatten() {
                socket::close(socket).await;
                shared.counters.closed();
            }
            return;
        }
    };
    let caller_user = caller.1.user;
    let callee_user = callee.1.user;
    let mut caller = Link::new(caller.0, caller.1);
    let mut callee = Link::new(callee.0, callee.1);
    let lost = drive(&shared, &mut caller, &mut callee, caller_user, callee_user).await;
    match lost {
        None => {}
        Some(Lost::Closed(code)) => {
            shared.closes.record(code);
            bump(&shared.counters.disconnects);
            stop_for(&shared.counters, code);
        }
        Some(Lost::PongTimeout) => {
            bump(&shared.counters.pong_timeouts);
            bump(&shared.counters.disconnects);
        }
        Some(Lost::Undecodable) => {
            bump(&shared.counters.protocol_failures);
            bump(&shared.counters.disconnects);
        }
    }
    socket::close(caller.socket).await;
    socket::close(callee.socket).await;
    shared.counters.closed();
    shared.counters.closed();
}

/// The pair's calls until the run ends and its live call is over, or a connection is lost.
async fn drive(
    shared: &Shared,
    caller: &mut Link,
    callee: &mut Link,
    caller_user: Uuid,
    callee_user: Uuid,
) -> Option<Lost> {
    let mut calls = PairCalls::new(
        &shared.calls,
        caller_user,
        callee_user,
        &shared.options,
        shared.last_invite,
        Instant::now(),
    );
    let mut stop_by: Option<Instant> = None;
    loop {
        let wake = calls.wake_at();
        let far = Instant::now() + Duration::from_secs(86_400);
        let outgoing = tokio::select! {
            () = shared.cancel.cancelled(), if stop_by.is_none() => {
                stop_by = Some(Instant::now() + HANGUP_GRACE);
                calls.stop(Instant::now())
            }
            () = tokio::time::sleep_until(stop_by.unwrap_or(far)), if stop_by.is_some() => {
                calls.out_of_time();
                return None;
            }
            () = tokio::time::sleep_until(wake.unwrap_or(far)), if wake.is_some() => {
                calls.on_timer(Instant::now())
            }
            () = tokio::time::sleep_until(caller.pong_due()) => return Some(Lost::PongTimeout),
            () = tokio::time::sleep_until(callee.pong_due()) => return Some(Lost::PongTimeout),
            _ = caller.pings.tick() => ping(shared, caller, Side::Caller),
            _ = callee.pings.tick() => ping(shared, callee, Side::Callee),
            message = caller.socket.next() => match receive(shared, caller, message) {
                Err(lost) => return Some(lost),
                Ok(None) => Vec::new(),
                Ok(Some(body)) => calls.on_frame(Side::Caller, body, Instant::now()),
            },
            message = callee.socket.next() => match receive(shared, callee, message) {
                Err(lost) => return Some(lost),
                Ok(None) => Vec::new(),
                Ok(Some(body)) => calls.on_frame(Side::Callee, body, Instant::now()),
            },
        };
        for (side, body) in outgoing {
            let link = match side {
                Side::Caller => &mut *caller,
                Side::Callee => &mut *callee,
            };
            if socket::send(&mut link.socket, body).await.is_err() {
                return Some(Lost::Closed(ABNORMAL_CLOSE));
            }
        }
        if stop_by.is_some() && calls.idle() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use push_proto::v1::{CallAccepted, CallEnded, CallError, CallIncoming, CallState, CallSync};

    fn options() -> CallsOptions {
        CallsOptions {
            calls: 1,
            ramp_per_second: 2,
            duration: Duration::from_secs(60),
            hold: Duration::from_secs(5),
            attempts_per_hour: 30,
            setup_p95: Duration::from_millis(300),
            target_build: "test".to_owned(),
            redis: None,
        }
    }

    fn synced(calls: &mut PairCalls<'_>, now: Instant) {
        for side in [Side::Caller, Side::Callee] {
            assert!(
                calls
                    .on_frame(side, Body::CallSync(CallSync::default()), now)
                    .is_empty()
            );
        }
    }

    fn invite_id(out: &[(Side, Body)]) -> Vec<u8> {
        match out {
            [(Side::Caller, Body::CallInvite(invite))] => {
                assert_eq!(invite.media, [i32::from(CallMedia::Audio)]);
                invite.call_id.clone()
            }
            other => panic!("expected one invite, got {other:?}"),
        }
    }

    fn ended(call_id: &[u8], reason: CallEndReason) -> Body {
        Body::CallEnded(CallEnded {
            call_id: call_id.to_vec(),
            reason: reason.into(),
            duration_s: None,
        })
    }

    #[test]
    fn a_pair_places_answers_holds_and_hangs_up_then_paces_the_next_call() {
        let stats = CallStats::default();
        let start = Instant::now();
        let callee = Uuid::now_v7();
        let mut calls = PairCalls::new(
            &stats,
            Uuid::now_v7(),
            callee,
            &options(),
            start + Duration::from_secs(45),
            start,
        );
        // Nothing before both CallSyncs (or the sync wait).
        assert_eq!(calls.wake_at(), Some(start + SYNC_WAIT));
        synced(&mut calls, start);
        assert_eq!(calls.wake_at(), Some(start));
        let id = invite_id(&calls.on_timer(start));
        assert_eq!(stats.live.now(), 1);

        let ringing = calls.on_frame(
            Side::Callee,
            Body::CallIncoming(CallIncoming {
                call_id: id.clone(),
                ..CallIncoming::default()
            }),
            start,
        );
        assert!(
            matches!(&ringing[..], [(Side::Callee, Body::CallAccept(accept)), (Side::Callee, Body::CallIce(_)), (Side::Callee, Body::CallIce(end))]
                if accept.call_id == id && end.end_of_candidates)
        );

        let accepted_at = start + Duration::from_millis(40);
        let trickled = calls.on_frame(
            Side::Caller,
            Body::CallAccepted(CallAccepted {
                call_id: id.clone(),
                ..CallAccepted::default()
            }),
            accepted_at,
        );
        assert_eq!(trickled.len(), 2);
        assert_eq!(stats.setup.sorted(), [40]);
        assert_eq!((stats.active.now(), stats.live.now()), (1, 1));
        assert_eq!(calls.wake_at(), Some(accepted_at + Duration::from_secs(5)));

        let hung_up = calls.on_timer(accepted_at + Duration::from_secs(5));
        assert!(matches!(&hung_up[..], [(Side::Caller, Body::CallHangup(h))] if h.call_id == id));
        let ending = accepted_at + Duration::from_secs(5);
        calls.on_frame(Side::Callee, ended(&id, CallEndReason::Completed), ending);
        assert!(!calls.idle(), "the caller's CallEnded is still due");
        calls.on_frame(Side::Caller, ended(&id, CallEndReason::Completed), ending);
        assert!(calls.idle());
        assert_eq!(read(&stats.completed), 1);
        assert_eq!(
            stats.ended.snapshot(),
            BTreeMap::from([("completed".to_owned(), 1)])
        );
        assert_eq!(
            stats.callee_ended.snapshot(),
            BTreeMap::from([("completed".to_owned(), 1)])
        );
        assert_eq!((stats.active.now(), stats.live.now()), (0, 0));
        assert_eq!((stats.active.peak(), stats.live.peak()), (1, 1));
        // The next invite comes a pause after the end (and a period after the last invite).
        assert_eq!(calls.wake_at(), Some(ending + CALL_PAUSE));
        assert_eq!(read(&stats.unexpected_frames), 0);
    }

    #[test]
    fn no_call_is_placed_after_the_last_invite_time_or_once_stopping() {
        let stats = CallStats::default();
        let start = Instant::now();
        let mut calls = PairCalls::new(
            &stats,
            Uuid::now_v7(),
            Uuid::now_v7(),
            &options(),
            start,
            start,
        );
        synced(&mut calls, start);
        assert!(calls.on_timer(start + Duration::from_secs(1)).is_empty());
        assert_eq!(calls.wake_at(), None);

        let mut stopped = PairCalls::new(
            &stats,
            Uuid::now_v7(),
            Uuid::now_v7(),
            &options(),
            start + Duration::from_secs(60),
            start,
        );
        synced(&mut stopped, start);
        assert!(stopped.stop(start).is_empty());
        assert!(stopped.idle() && stopped.wake_at().is_none());
        assert_eq!(read(&stats.placed), 0);
    }

    #[test]
    fn an_unanswered_call_is_hung_up_and_a_missing_ending_gives_up() {
        let stats = CallStats::default();
        let start = Instant::now();
        let mut calls = PairCalls::new(
            &stats,
            Uuid::now_v7(),
            Uuid::now_v7(),
            &options(),
            start + Duration::from_secs(45),
            start,
        );
        synced(&mut calls, start);
        let id = invite_id(&calls.on_timer(start));
        let late = start + SETUP_TIMEOUT;
        assert!(
            matches!(&calls.on_timer(late)[..], [(Side::Caller, Body::CallHangup(h))] if h.call_id == id)
        );
        assert_eq!(read(&stats.unanswered), 1);
        assert!(calls.on_timer(late + ENDED_TIMEOUT).is_empty());
        assert_eq!(read(&stats.endings_missing), 1);
        assert_eq!((stats.live.now(), calls.wake_at()), (0, None));
    }

    #[test]
    fn a_refused_invite_or_a_busy_callee_is_counted_and_the_pair_stops_after_an_error() {
        let stats = CallStats::default();
        let start = Instant::now();
        let mut calls = PairCalls::new(
            &stats,
            Uuid::now_v7(),
            Uuid::now_v7(),
            &options(),
            start + Duration::from_secs(45),
            start,
        );
        synced(&mut calls, start);
        let id = invite_id(&calls.on_timer(start));
        calls.on_frame(Side::Caller, ended(&id, CallEndReason::Busy), start);
        assert_eq!(
            stats.ended.snapshot(),
            BTreeMap::from([("busy".to_owned(), 1)])
        );
        assert!(calls.idle() && calls.wake_at().is_some());

        let next = calls.wake_at().unwrap();
        let id = invite_id(&calls.on_timer(next));
        calls.on_frame(
            Side::Caller,
            Body::CallError(CallError {
                call_id: id,
                code: ErrorCode::RateLimited.into(),
                ..CallError::default()
            }),
            next,
        );
        assert_eq!(
            stats.errors.snapshot(),
            BTreeMap::from([("rate_limited".to_owned(), 1)])
        );
        assert_eq!((stats.live.now(), calls.wake_at()), (0, None));
    }

    fn sync_of(call_id: &[u8], peer: Uuid) -> Body {
        Body::CallSync(CallSync {
            calls: vec![CallState {
                call_id: call_id.to_vec(),
                peer_user_id: id_bytes(peer),
                this_device: true,
                ..CallState::default()
            }],
        })
    }

    fn not_found(call_id: &[u8]) -> Body {
        Body::CallError(CallError {
            call_id: call_id.to_vec(),
            code: ErrorCode::CallNotFound.into(),
            ..CallError::default()
        })
    }

    #[test]
    fn a_leftover_call_of_the_pair_is_hung_up_once_by_the_caller_and_both_endings_absorbed() {
        for callee_first in [false, true] {
            let stats = CallStats::default();
            let start = Instant::now();
            let (caller, callee) = (Uuid::now_v7(), Uuid::now_v7());
            let mut calls = PairCalls::new(
                &stats,
                caller,
                callee,
                &options(),
                start + Duration::from_secs(45),
                start,
            );
            let stale = id_bytes(Uuid::now_v7());
            let mut out = Vec::new();
            if callee_first {
                out.extend(calls.on_frame(Side::Callee, sync_of(&stale, caller), start));
                out.extend(calls.on_frame(Side::Caller, sync_of(&stale, callee), start));
            } else {
                out.extend(calls.on_frame(Side::Caller, sync_of(&stale, callee), start));
                out.extend(calls.on_frame(Side::Callee, sync_of(&stale, caller), start));
            }
            assert!(
                matches!(&out[..], [(Side::Caller, Body::CallHangup(h))] if h.call_id == stale),
                "callee first {callee_first}: {out:?}"
            );
            assert_eq!(
                calls.wake_at(),
                Some(start + SYNC_WAIT),
                "waits for the leftover to end"
            );
            calls.on_frame(Side::Callee, ended(&stale, CallEndReason::Completed), start);
            assert_eq!(
                calls.wake_at(),
                Some(start + SYNC_WAIT),
                "the caller's is due"
            );
            calls.on_frame(Side::Caller, ended(&stale, CallEndReason::Completed), start);
            // A hangup that crossed the ending is refused: not an error of the run.
            calls.on_frame(Side::Callee, not_found(&stale), start);
            assert_eq!(calls.wake_at(), Some(start));
            assert_eq!(read(&stats.stale_calls), 1);
            assert_eq!(read(&stats.unexpected_frames), 0);
            assert!(stats.errors.snapshot().is_empty());
            assert!(
                stats.ended.snapshot().is_empty() && stats.callee_ended.snapshot().is_empty(),
                "a leftover's ending is not counted"
            );
            invite_id(&calls.on_timer(start));
        }
    }

    #[test]
    fn a_leftover_call_with_someone_else_is_hung_up_by_its_own_device_and_strays_are_unexpected() {
        let stats = CallStats::default();
        let start = Instant::now();
        let caller = Uuid::now_v7();
        let mut calls = PairCalls::new(
            &stats,
            caller,
            Uuid::now_v7(),
            &options(),
            start + Duration::from_secs(45),
            start,
        );
        let stale = id_bytes(Uuid::now_v7());
        let out = calls.on_frame(Side::Callee, sync_of(&stale, Uuid::now_v7()), start);
        assert!(matches!(&out[..], [(Side::Callee, Body::CallHangup(h))] if h.call_id == stale));
        // A leftover with the caller that the caller's device does not hold.
        let other = id_bytes(Uuid::now_v7());
        assert!(
            calls
                .on_frame(Side::Callee, sync_of(&other, caller), start)
                .is_empty()
        );
        let out = calls.on_frame(Side::Caller, Body::CallSync(CallSync::default()), start);
        assert!(matches!(&out[..], [(Side::Callee, Body::CallHangup(h))] if h.call_id == other));
        calls.on_frame(Side::Callee, ended(&stale, CallEndReason::Completed), start);
        calls.on_frame(Side::Callee, ended(&other, CallEndReason::Completed), start);
        assert_eq!(calls.wake_at(), Some(start));
        assert_eq!(read(&stats.stale_calls), 2);

        calls.on_frame(Side::Callee, ended(&stale, CallEndReason::Completed), start);
        calls.on_frame(Side::Caller, ended(&stale, CallEndReason::Completed), start);
        calls.on_frame(
            Side::Callee,
            Body::CallAccepted(CallAccepted::default()),
            start,
        );
        assert_eq!(read(&stats.unexpected_frames), 3);
    }

    #[test]
    fn a_call_still_live_when_the_stop_grace_runs_out_is_an_ending_missing() {
        let stats = CallStats::default();
        let start = Instant::now();
        let mut calls = PairCalls::new(
            &stats,
            Uuid::now_v7(),
            Uuid::now_v7(),
            &options(),
            start + Duration::from_secs(45),
            start,
        );
        synced(&mut calls, start);
        let id = invite_id(&calls.on_timer(start));
        calls.on_frame(
            Side::Caller,
            Body::CallAccepted(CallAccepted {
                call_id: id.clone(),
                ..CallAccepted::default()
            }),
            start,
        );
        assert!(
            matches!(&calls.stop(start)[..], [(Side::Caller, Body::CallHangup(h))] if h.call_id == id)
        );
        calls.out_of_time();
        assert_eq!(read(&stats.endings_missing), 1);
        assert_eq!((stats.live.now(), stats.active.now()), (0, 0));
        assert!(calls.idle());
        // Nothing live: nothing missing.
        calls.out_of_time();
        assert_eq!(read(&stats.endings_missing), 1);
    }

    #[test]
    fn reasons_and_codes_are_named_for_the_report() {
        assert_eq!(
            reason_name(CallEndReason::Unreachable.into()),
            "unreachable"
        );
        assert_eq!(code_name(ErrorCode::NotAvailable.into()), "not_available");
        assert_eq!(reason_name(99), "99");
    }
}
