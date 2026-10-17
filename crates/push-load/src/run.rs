//! A measured run. It refreshes the selected devices' tokens once (saving the rotated
//! refresh tokens before connecting), opens their connections at a fixed rate, keeps
//! them alive with application Pings every announced heartbeat for the duration, sends
//! sample messages between the prepared pairs, reconnects connections the target drops
//! (a gateway rollout), then closes every socket and reports. It never re-authenticates,
//! discovers targets or creates accounts.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, ErrorCode, Ping, Send, receipt};
use push_proto::{decode_frame, id_bytes};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{RunOptions, Target};
use crate::http::{ApiError, AuthApi};
use crate::metrics::{
    self, CloseCodes, Counters, LAG_LIMIT, Messages, PERCENTILES, Samples, bump, read,
};
use crate::report::{self, Connections, Generator, Latency, Report, Status, Workload};
use crate::socket::{self, ABNORMAL_CLOSE, Opened, Ready, Socket};
use crate::state::{DeviceRef, State};

const REFRESH_CONCURRENCY: usize = 32;
/// Tokens are refreshed right before connecting; a slower refresh would eat into their life.
const REFRESH_PHASE_LIMIT: Duration = Duration::from_secs(180);
const SAVE_EVERY: usize = 256;
/// A connection must reach Ready within this long of its first attempt or of losing it.
const READY_DEADLINE: Duration = Duration::from_secs(60);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// No sample message is sent in the last seconds of the run, so all can arrive.
const DRAIN: Duration = Duration::from_secs(10);
const STOP_GRACE: Duration = Duration::from_secs(15);
/// Acks are coalesced (the highest seq per conversation) and sent this often, inside the
/// gateway's frame budget even for a large backlog.
const ACK_EVERY: Duration = Duration::from_millis(250);
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    Reconnect,
    AuthFailure,
    Takeover,
    RateLimited,
}

/// What a close code means for a connection (`push_proto::close`).
pub fn close_action(code: u16) -> CloseAction {
    match code {
        4001..=4003 => CloseAction::AuthFailure,
        4004 => CloseAction::RateLimited,
        4007 => CloseAction::Takeover,
        _ => CloseAction::Reconnect,
    }
}

/// `base` scaled by a random factor from 0.5 to 1.5.
fn jitter(base: Duration) -> Duration {
    let random = Uuid::new_v4().as_bytes()[0];
    base.mul_f64(0.5 + f64::from(random) / 255.0)
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Cancelled,
    Closed(u16),
    PongTimeout,
    Undecodable,
}

struct Shared {
    target: Target,
    options: RunOptions,
    cancel: CancellationToken,
    stop_sending: AtomicBool,
    heartbeat_ms: AtomicU64,
    counters: Counters,
    closes: CloseCodes,
    refusals: CloseCodes,
    connect: Samples,
    ping_rtt: Samples,
    reconnect: Samples,
    messages: Messages,
}

struct Plan {
    access_token: String,
    /// The conversation a pair's sender sends sample messages to.
    sends_to: Option<Vec<u8>>,
}

pub async fn run(
    target: &Target,
    state_path: &Path,
    options: &RunOptions,
) -> anyhow::Result<Report> {
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
    let ready = state.ready_devices();
    if ready.len() < options.connections {
        bail!(
            "only {} prepared devices are ready; --connections {} needs more (run prepare with more accounts or devices)",
            ready.len(),
            options.connections
        );
    }
    let selected: Vec<DeviceRef> = ready[..options.connections].to_vec();
    let chosen: HashSet<DeviceRef> = selected.iter().copied().collect();
    let sends_to: HashMap<DeviceRef, Vec<u8>> = state
        .pairs
        .iter()
        .filter(|pair| {
            state.pair_is_ready(pair)
                && chosen.contains(&pair.sender)
                && chosen.contains(&pair.receiver)
        })
        .map(|pair| (pair.sender, id_bytes(pair.conversation_id)))
        .collect();
    let started_unix_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let started = Instant::now();
    let shared = Arc::new(Shared {
        target: target.clone(),
        options: options.clone(),
        cancel: CancellationToken::new(),
        stop_sending: AtomicBool::new(false),
        heartbeat_ms: AtomicU64::new(0),
        counters: Counters::default(),
        closes: CloseCodes::default(),
        refusals: CloseCodes::default(),
        connect: Samples::default(),
        ping_rtt: Samples::default(),
        reconnect: Samples::default(),
        messages: Messages::default(),
    });

    let tokens = refresh_all(target, &mut state, state_path, &selected, &shared).await?;

    let interrupt = CancellationToken::new();
    let on_interrupt = interrupt.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_interrupt.cancel();
        }
    });
    let lag = Arc::new(Samples::default());
    let lag_cancel = CancellationToken::new();
    let lag_watch = tokio::spawn(metrics::watch_lag(lag.clone(), lag_cancel.clone()));

    let mut tasks = JoinSet::new();
    let ramp_start = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / f64::from(options.ramp_per_second));
    for (index, (device, token)) in selected.iter().zip(tokens).enumerate() {
        let due = ramp_start + interval.mul_f64(index as f64);
        tokio::select! {
            () = interrupt.cancelled() => break,
            () = tokio::time::sleep_until(due) => {}
        }
        // A device whose refresh failed is counted already and not connected.
        let Some(access_token) = token else { continue };
        let plan = Plan {
            access_token,
            sends_to: sends_to.get(device).cloned(),
        };
        tasks.spawn(connection(shared.clone(), plan));
    }
    let ramp_end = Instant::now();

    let mut min_active: Option<i64> = None;
    if !interrupt.is_cancelled() {
        let run_end = ramp_end + options.duration;
        let stop_sending_at = (run_end - DRAIN).max(ramp_end);
        let mut sampler = tokio::time::interval(Duration::from_secs(1));
        sampler.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = interrupt.cancelled() => break,
                () = tokio::time::sleep_until(run_end) => break,
                _ = sampler.tick() => {
                    if Instant::now() >= stop_sending_at {
                        shared.stop_sending.store(true, Ordering::SeqCst);
                    }
                    let active = shared.counters.active();
                    min_active = Some(min_active.map_or(active, |least| least.min(active)));
                }
            }
        }
    }
    shared.stop_sending.store(true, Ordering::SeqCst);
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

    let c = &shared.counters;
    let lag = lag.summary();
    let heartbeat_ms = shared.heartbeat_ms.load(Ordering::SeqCst);
    let mut report = Report {
        tool: "push-load".to_owned(),
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        status: Status::Failed,
        failures: Vec::new(),
        target: target.base_url(),
        target_build: options.target_build.trim().to_owned(),
        started_unix_s,
        elapsed_s: started.elapsed().as_secs_f64(),
        workload: Workload {
            connections: options.connections,
            ramp_per_second: options.ramp_per_second,
            ramp_s: (ramp_end - ramp_start).as_secs_f64(),
            duration_s: options.duration.as_secs(),
            heartbeat_ms: (heartbeat_ms > 0).then_some(heartbeat_ms),
            sample_pairs: sends_to.len(),
            message_interval_s: options.message_interval.as_secs(),
            reconnect: options.reconnect,
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
        messages: shared.messages.counts(),
        latency: Latency {
            connect: shared.connect.summary(),
            ping_rtt: shared.ping_rtt.summary(),
            delivery: shared.messages.delivery.summary(),
            receipt: shared.messages.receipt.summary(),
            reconnect: shared.reconnect.summary(),
        },
        generator: Generator {
            saturated: lag
                .p99_ms
                .is_some_and(|p99| u128::from(p99) > LAG_LIMIT.as_millis()),
            scheduling_lag: lag,
            lag_limit_ms: u64::try_from(LAG_LIMIT.as_millis()).unwrap_or(u64::MAX),
        },
        percentiles: PERCENTILES.to_owned(),
    };
    let (status, failures) = report::judge(&report, interrupt.is_cancelled());
    report.status = status;
    report.failures = failures;
    Ok(report)
}

/// One refresh per selected device: the access tokens, in `selected` order (`None` when
/// refused). Rotated refresh tokens are saved before any connection uses them; a refused
/// one marks its device for `prepare` to sign in again.
async fn refresh_all(
    target: &Target,
    state: &mut State,
    path: &Path,
    selected: &[DeviceRef],
    shared: &Shared,
) -> anyhow::Result<Vec<Option<String>>> {
    let api = AuthApi::new(target)?;
    let deadline = Instant::now() + REFRESH_PHASE_LIMIT;
    let limit = Arc::new(Semaphore::new(REFRESH_CONCURRENCY));
    let mut jobs = JoinSet::new();
    for (position, device) in selected.iter().enumerate() {
        let token = state
            .device(*device)
            .refresh_token
            .clone()
            .context("a ready device has no refresh token")?;
        let (api, limit) = (api.clone(), limit.clone());
        jobs.spawn(async move {
            let _permit = limit
                .acquire_owned()
                .await
                .expect("the semaphore stays open");
            (position, api.refresh(&token).await)
        });
    }
    let mut access = vec![None; selected.len()];
    let mut unsaved = 0;
    loop {
        let joined = match tokio::time::timeout_at(deadline, jobs.join_next()).await {
            Ok(Some(joined)) => joined.context("a refresh task panicked")?,
            Ok(None) => break,
            Err(_) => {
                jobs.abort_all();
                while jobs.join_next().await.is_some() {}
                state.save(path)?;
                bail!(
                    "refreshing the devices' tokens took longer than {} s; nothing was connected. Devices whose answers were cut off need `prepare` again after 30 s",
                    REFRESH_PHASE_LIMIT.as_secs()
                );
            }
        };
        let (position, outcome) = joined;
        let device = selected[position];
        match outcome {
            Ok(tokens) => {
                state.device_mut(device).refresh_token = Some(tokens.refresh_token);
                access[position] = Some(tokens.access_token);
            }
            Err(ApiError::Rejected { status: 401, .. }) => {
                state.device_mut(device).needs_sign_in = true;
                bump(&shared.counters.auth_failures);
            }
            Err(_) => bump(&shared.counters.connect_failures),
        }
        unsaved += 1;
        if unsaved >= SAVE_EVERY {
            state.save(path)?;
            unsaved = 0;
        }
    }
    state.save(path)?;
    Ok(access)
}

/// One device's connection for the whole run: connect, keep it alive, reconnect when the
/// target drops it, stop on cancellation or on a close that must not be retried.
async fn connection(shared: Arc<Shared>, plan: Plan) {
    let first_attempt = Instant::now();
    let mut ever_ready = false;
    let mut lost_at: Option<Instant> = None;
    let mut deadline = first_attempt + READY_DEADLINE;
    let mut backoff = FIRST_BACKOFF;
    loop {
        let opened = tokio::select! {
            () = shared.cancel.cancelled() => return,
            opened = socket::open(&shared.target, &plan.access_token) => opened,
        };
        match opened {
            Ok(Opened::Ready(socket, ready)) => {
                if ever_ready {
                    if let Some(at) = lost_at.take() {
                        shared.reconnect.record(at.elapsed());
                        bump(&shared.counters.reconnects);
                    }
                } else {
                    ever_ready = true;
                    shared.connect.record(first_attempt.elapsed());
                    bump(&shared.counters.ready_once);
                }
                let heartbeat = u64::try_from(ready.heartbeat.as_millis()).unwrap_or(u64::MAX);
                shared.heartbeat_ms.store(heartbeat, Ordering::SeqCst);
                shared.counters.opened();
                let ended = session(&shared, &plan, *socket, ready).await;
                shared.counters.closed();
                match ended {
                    Ended::Cancelled => return,
                    Ended::Closed(code) => {
                        shared.closes.record(code);
                        bump(&shared.counters.disconnects);
                        if stop_for(&shared, code) {
                            return;
                        }
                    }
                    Ended::PongTimeout => {
                        bump(&shared.counters.pong_timeouts);
                        bump(&shared.counters.disconnects);
                    }
                    Ended::Undecodable => {
                        bump(&shared.counters.protocol_failures);
                        bump(&shared.counters.disconnects);
                    }
                }
                if !shared.options.reconnect {
                    return;
                }
                lost_at = Some(Instant::now());
                deadline = Instant::now() + READY_DEADLINE;
                backoff = FIRST_BACKOFF;
            }
            Ok(Opened::Closed(code)) => {
                shared.refusals.record(code);
                if stop_for(&shared, code) {
                    return;
                }
            }
            Err(_) => bump(&shared.counters.attempt_errors),
        }
        if Instant::now() >= deadline {
            bump(if ever_ready {
                &shared.counters.reconnect_failures
            } else {
                &shared.counters.connect_failures
            });
            return;
        }
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            () = tokio::time::sleep(jitter(backoff)) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Counts a close that must not be retried; whether the connection stops.
fn stop_for(shared: &Shared, code: u16) -> bool {
    let counter = match close_action(code) {
        CloseAction::Reconnect => return false,
        CloseAction::AuthFailure => &shared.counters.auth_failures,
        CloseAction::Takeover => &shared.counters.takeovers,
        CloseAction::RateLimited => &shared.counters.rate_limited,
    };
    bump(counter);
    true
}

/// An identified connection until it ends: application Pings every heartbeat, sample
/// messages for a pair's sender, coalesced Acks for everything delivered.
async fn session(shared: &Shared, plan: &Plan, mut socket: Socket, ready: Ready) -> Ended {
    let own = id_bytes(ready.user);
    let now = Instant::now();
    let mut pings = tokio::time::interval_at(now + ready.heartbeat, ready.heartbeat);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let every = shared.options.message_interval;
    let mut sends = tokio::time::interval_at(now + every, every);
    sends.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut acks = tokio::time::interval(ACK_EVERY);
    acks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut to_ack: HashMap<Vec<u8>, i64> = HashMap::new();
    let mut ping_sent: Option<Instant> = None;
    loop {
        let pong_due = ping_sent.map_or_else(
            || Instant::now() + Duration::from_secs(86_400),
            |sent| sent + PONG_TIMEOUT,
        );
        let outgoing = tokio::select! {
            () = shared.cancel.cancelled() => {
                socket::close(socket).await;
                return Ended::Cancelled;
            }
            () = tokio::time::sleep_until(pong_due) => return Ended::PongTimeout,
            _ = pings.tick() => {
                if ping_sent.is_some() {
                    continue;
                }
                ping_sent = Some(Instant::now());
                bump(&shared.counters.pings);
                vec![Body::Ping(Ping {})]
            }
            _ = sends.tick(), if plan.sends_to.is_some() => {
                if shared.stop_sending.load(Ordering::SeqCst) {
                    continue;
                }
                let conversation = plan.sends_to.clone().expect("guarded by the branch condition");
                let client_msg_id = id_bytes(Uuid::now_v7());
                shared.messages.sent(client_msg_id.clone(), conversation.clone());
                vec![Body::Send(Send {
                    client_msg_id,
                    conversation_id: conversation,
                    content_type: ContentType::Text.into(),
                    body: format!("push-load {}", Uuid::now_v7()).into_bytes(),
                    client_ts: None,
                })]
            }
            _ = acks.tick() => to_ack
                .drain()
                .map(|(conversation_id, seq)| Body::Ack(Ack { conversation_id, seq }))
                .collect(),
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        let Ok(body) = decode_frame(&bytes) else { return Ended::Undecodable };
                        match body {
                            Body::Pong(_) => {
                                if let Some(sent) = ping_sent.take() {
                                    shared.ping_rtt.record(sent.elapsed());
                                    bump(&shared.counters.pongs);
                                }
                            }
                            Body::SendAck(ack) => shared.messages.stored(&ack.client_msg_id, ack.seq),
                            Body::Deliver(entry) => {
                                if entry.sender_id != own {
                                    shared.messages.delivered(&entry.client_msg_id);
                                }
                                let highest = to_ack.entry(entry.conversation_id).or_insert(entry.seq);
                                *highest = (*highest).max(entry.seq);
                            }
                            Body::Receipt(receipt) => {
                                if let Some(receipt::Positions::Direct(direct)) = receipt.positions {
                                    shared.messages.receipted(&receipt.conversation_id, direct.delivered_seq);
                                }
                            }
                            Body::AuthExpiring(_) => bump(&shared.counters.auth_expiring),
                            Body::Error(error) => {
                                bump(&shared.counters.error_frames);
                                if error.code() == ErrorCode::RateLimited {
                                    bump(&shared.counters.rate_limited);
                                }
                            }
                            _ => {}
                        }
                        Vec::new()
                    }
                    Some(Ok(Message::Close(frame))) => return Ended::Closed(socket::close_code(frame)),
                    Some(Ok(_)) => Vec::new(),
                    Some(Err(_)) | None => return Ended::Closed(ABNORMAL_CLOSE),
                }
            }
        };
        for body in outgoing {
            if socket::send(&mut socket, body).await.is_err() {
                return Ended::Closed(ABNORMAL_CLOSE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_decide_whether_a_connection_reconnects() {
        for code in [4001, 4002, 4003] {
            assert_eq!(close_action(code), CloseAction::AuthFailure, "{code}");
        }
        assert_eq!(close_action(4004), CloseAction::RateLimited);
        assert_eq!(close_action(4007), CloseAction::Takeover);
        for code in [1000, 1001, 1002, 1006, 1008, 1011, 4005, 4006] {
            assert_eq!(close_action(code), CloseAction::Reconnect, "{code}");
        }
    }

    #[test]
    fn jitter_stays_between_half_and_one_and_a_half() {
        for _ in 0..100 {
            let pause = jitter(Duration::from_secs(2));
            assert!(
                (Duration::from_secs(1)..=Duration::from_secs(3)).contains(&pause),
                "{pause:?}"
            );
        }
    }
}
