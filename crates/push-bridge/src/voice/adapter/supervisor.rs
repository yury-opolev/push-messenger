//! The voice adapter's supervisor (design §4, §5, A18–A20): starts the adapter (spawn or
//! connect), opens a [`Link`], publishes it while it lives, and restarts it with backoff
//! (1 s doubling to 60 s, reset after [`RESET_AFTER`] ready) when it ends.
//!
//! States ([`AdapterState`]): `Off` (no `calls.adapter`, or shut down) → `Starting`
//! (spawning or connecting) → `Handshaking` (hello sent) → `Ready` → on any end →
//! `Backoff {until, reason}` → `Starting`. A shut-down supervisor ends `Off`.
//!
//! What the core gets ([`Supervisor`]):
//!
//! - [`Supervisor::state`]: a `watch` of the state, for advertising (A20), `status` and the
//!   tray; clone it freely.
//! - [`Supervisor::current`]: a `watch` of the ready link, a [`ReadyLink`] (or `None`).
//!   Each link's event receiver has exactly one consumer: the call router takes it
//!   ([`ReadyLink::take_events`]) and may give it back ([`ReadyLink::give_back`]) when it
//!   stops reading (no call). While nobody holds it, the supervisor drains it every
//!   [`DRAIN_INTERVAL`] (events outside a call are dropped, logged at debug by type), so
//!   nothing piles up. A new `ReadyLink` (a new [`ReadyLink::id`]) mid-call means the
//!   router must send `call_started` again (A21).
//! - [`Supervisor::shutdown`]: closes the link (queued control messages, e.g. the
//!   router's `call_ended {shutdown}`, are flushed first), waits up to [`STOP_GRACE`] for a
//!   spawned child, kills it, and ends `Off`.
//! - [`Supervisor::restarts`]: how many times the adapter was started again
//!   (`adapter_restarts`).
//!
//! How the adapter is started is a [`Starter`]: [`starter_for`] gives the real ones
//! (spawn, connect); tests use the testkit's scripted adapter over in-memory pipes.
//!
//! Every failure (a spawn or connect error, a failed handshake, a link that ended) is one
//! `warn` line `event = "voice_adapter_failed"` with a content-free reason (a refused
//! format names `unsupported_format`); the same reason again within
//! [`REPEAT_LOG_INTERVAL`] is logged at debug.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use push_voice_adapter_proto::BridgeHello;
use push_voice_adapter_proto::message::Message as _;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Child;
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout, timeout_at};

use super::link::{AdapterInfo, Link, LinkEvent};
use super::spawn::{KillGuard, spawn};
use crate::backoff::{Backoff, RESET_AFTER};
use crate::config::{AdapterConfig, ConnectConfig, SpawnConfig};

/// How long a spawned adapter may take to exit once its link closed, before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(2);
/// How often an event receiver nobody took is drained.
pub const DRAIN_INTERVAL: Duration = Duration::from_secs(1);
/// The same failure reason is logged as a warning at most this often.
pub const REPEAT_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Where the adapter client is.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterState {
    /// No adapter configured, or shut down.
    Off,
    /// Spawning or connecting.
    Starting,
    /// Hello sent, waiting for the adapter's.
    Handshaking,
    /// The adapter is ready; its hello.
    Ready(AdapterInfo),
    /// Waiting to start again; `reason` is content-free.
    Backoff { until: Instant, reason: String },
}

impl AdapterState {
    /// The adapter's hello while ready.
    pub fn ready(&self) -> Option<&AdapterInfo> {
        match self {
            AdapterState::Ready(info) => Some(info),
            _ => None,
        }
    }
}

/// The adapter's side of the protocol: its frames in, the bridge's frames out.
pub type BoxReader = Box<dyn AsyncRead + Send + Unpin>;
/// The bridge's frames to the adapter.
pub type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// A started adapter: the streams to speak the protocol on and, in spawn mode, the child.
pub struct Started {
    pub reader: BoxReader,
    pub writer: BoxWriter,
    pub child: Option<ChildGuard>,
}

impl fmt::Debug for Started {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Started")
            .field("child", &self.child.is_some())
            .finish_non_exhaustive()
    }
}

/// How the supervisor starts an adapter. A failure's message is content-free and is the
/// backoff's reason.
#[async_trait]
pub trait Starter: Send + Sync + 'static {
    async fn start(&self) -> io::Result<Started>;
}

/// A spawned adapter process; [`ChildGuard::finish`] (or dropping it) ends it.
pub struct ChildGuard {
    child: Child,
    _job: KillGuard,
}

impl ChildGuard {
    pub fn new(child: Child, job: KillGuard) -> Self {
        Self { child, _job: job }
    }

    /// Waits up to `grace` for the child to exit (its stdin is closed by then), then kills
    /// it; returns once it is gone.
    pub async fn finish(mut self, grace: Duration) {
        if timeout(grace, self.child.wait()).await.is_ok() {
            return;
        }
        tracing::debug!(event = "voice_adapter_killed");
        if let Err(error) = self.child.kill().await {
            tracing::warn!(event = "voice_adapter_kill_failed", kind = %error.kind());
        }
    }
}

/// Starts the adapter `calls.adapter.spawn` describes.
#[derive(Debug, Clone)]
pub struct SpawnStarter {
    pub config: SpawnConfig,
    pub log_dir: PathBuf,
    pub extra_secret_names: Vec<String>,
}

#[async_trait]
impl Starter for SpawnStarter {
    async fn start(&self) -> io::Result<Started> {
        let spawned = spawn(&self.config, &self.log_dir, &self.extra_secret_names)?;
        Ok(Started {
            reader: Box::new(spawned.stdout),
            writer: Box::new(spawned.stdin),
            child: Some(ChildGuard::new(spawned.child, spawned.job)),
        })
    }
}

/// Connects to the adapter `calls.adapter.connect` names.
#[derive(Debug, Clone)]
pub struct ConnectStarter {
    pub config: ConnectConfig,
}

#[async_trait]
impl Starter for ConnectStarter {
    async fn start(&self) -> io::Result<Started> {
        let stream = super::connect::connect(&self.config).await?;
        let (reader, writer) = tokio::io::split(stream);
        Ok(Started {
            reader: Box::new(reader),
            writer: Box::new(writer),
            child: None,
        })
    }
}

/// The real starter for `config`. `extra_secret_names`: the variables the bridge's config
/// names as key sources, kept from a spawned adapter.
pub fn starter_for(
    config: AdapterConfig,
    log_dir: PathBuf,
    extra_secret_names: Vec<String>,
) -> Arc<dyn Starter> {
    match config {
        AdapterConfig::Spawn(config) => Arc::new(SpawnStarter {
            config,
            log_dir,
            extra_secret_names,
        }),
        AdapterConfig::Connect(config) => Arc::new(ConnectStarter { config }),
    }
}

/// The ready link and its event receiver, which one consumer takes.
#[derive(Clone)]
pub struct ReadyLink {
    id: u64,
    link: Link,
    events: Arc<Mutex<Option<mpsc::UnboundedReceiver<LinkEvent>>>>,
    /// Signalled by [`ReadyLink::give_back`].
    returned: Arc<Notify>,
}

impl fmt::Debug for ReadyLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyLink")
            .field("id", &self.id)
            .field("link", &self.link)
            .finish()
    }
}

impl ReadyLink {
    /// Distinct for every link this supervisor opened (increasing).
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    /// The link's events, once: `None` when someone already holds them. The last event is
    /// [`LinkEvent::Closed`] unless the supervisor drained it while nobody held the
    /// receiver; a closed channel means the link ended ([`Link::end`]).
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<LinkEvent>> {
        lock(&self.events).take()
    }

    /// Returns the receiver taken with [`take_events`](Self::take_events); the supervisor
    /// drains it again until it is taken.
    pub fn give_back(&self, events: mpsc::UnboundedReceiver<LinkEvent>) {
        *lock(&self.events) = Some(events);
        self.returned.notify_waiters();
    }

    /// The link's events, waiting up to `wait` for the consumer holding them (the
    /// previous call's router, finishing) to give them back.
    pub async fn wait_for_events(
        &self,
        wait: Duration,
    ) -> Option<mpsc::UnboundedReceiver<LinkEvent>> {
        let deadline = Instant::now() + wait;
        loop {
            let returned = self.returned.notified();
            tokio::pin!(returned);
            returned.as_mut().enable();
            if let Some(events) = self.take_events() {
                return Some(events);
            }
            if timeout_at(deadline, returned).await.is_err() {
                return self.take_events();
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Drops whatever arrived on an event receiver nobody holds.
fn drain(slot: &Mutex<Option<mpsc::UnboundedReceiver<LinkEvent>>>) {
    let mut slot = lock(slot);
    let Some(events) = slot.as_mut() else {
        return;
    };
    while let Ok(event) = events.try_recv() {
        if let LinkEvent::Message(message) = &event {
            tracing::debug!(
                event = "voice_adapter_event_dropped",
                message_type = message.message_type()
            );
        }
    }
}

/// The supervisor of the voice adapter. Cloning is cheap; when every clone is dropped the
/// adapter is stopped as by [`Supervisor::shutdown`].
#[derive(Clone)]
pub struct Supervisor {
    state: watch::Receiver<AdapterState>,
    current: watch::Receiver<Option<ReadyLink>>,
    restarts: Arc<AtomicU64>,
    stop: Arc<watch::Sender<bool>>,
}

impl fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Supervisor")
            .field("state", &*self.state.borrow())
            .field("restarts", &self.restarts())
            .finish()
    }
}

/// Supervises the adapter `config` describes (call inside a tokio runtime). `log_dir`: the
/// spawned adapter's stderr log folder; `extra_secret_names`: the variables the bridge's
/// config names as key sources (e.g. Azure `key_env`), never passed to a spawned adapter.
pub fn start(
    config: AdapterConfig,
    hello: BridgeHello,
    log_dir: PathBuf,
    extra_secret_names: Vec<String>,
) -> Supervisor {
    if let AdapterConfig::Spawn(spawn) = &config {
        warn_relative(spawn);
    }
    Supervisor::with_starter(starter_for(config, log_dir, extra_secret_names), hello)
}

/// A program path relative to the bridge's working directory (not a bare name, which
/// the OS looks up on PATH) depends on where the bridge was started.
fn warn_relative(config: &SpawnConfig) {
    let Some(program) = config.command.first() else {
        return;
    };
    let path = Path::new(program);
    if path.is_relative() && path.components().count() > 1 {
        tracing::warn!(
            event = "voice_adapter_relative_path",
            "calls.adapter.spawn.command[0] is a relative path; it is resolved against the \
             bridge's working directory. Use an absolute path."
        );
    }
}

impl Supervisor {
    /// No adapter (`calls.adapter` absent): always `Off`.
    pub fn off() -> Supervisor {
        let (_, state) = watch::channel(AdapterState::Off);
        let (_, current) = watch::channel(None);
        let (stop, _) = watch::channel(true);
        Supervisor {
            state,
            current,
            restarts: Arc::default(),
            stop: Arc::new(stop),
        }
    }

    /// Supervises adapters `starter` starts (call inside a tokio runtime).
    pub fn with_starter(starter: Arc<dyn Starter>, hello: BridgeHello) -> Supervisor {
        Self::launch(starter, hello, None)
    }

    fn launch(
        starter: Arc<dyn Starter>,
        hello: BridgeHello,
        observer: Option<mpsc::UnboundedSender<(Instant, AdapterState)>>,
    ) -> Supervisor {
        let (state_tx, state) = watch::channel(AdapterState::Starting);
        let (current_tx, current) = watch::channel(None);
        let (stop, stop_rx) = watch::channel(false);
        let restarts = Arc::new(AtomicU64::new(0));
        let runner = Runner {
            starter,
            hello,
            state: state_tx,
            current: current_tx,
            restarts: Arc::clone(&restarts),
            stop: stop_rx,
            observer,
            backoff: Backoff::new(),
            failures: FailureLog::default(),
            next_id: 0,
        };
        tokio::spawn(runner.run());
        Supervisor {
            state,
            current,
            restarts,
            stop: Arc::new(stop),
        }
    }

    /// The state, to watch.
    pub fn state(&self) -> watch::Receiver<AdapterState> {
        self.state.clone()
    }

    /// The ready link, to watch.
    pub fn current(&self) -> watch::Receiver<Option<ReadyLink>> {
        self.current.clone()
    }

    /// How many times the adapter was started again after the first start.
    pub fn restarts(&self) -> u64 {
        self.restarts.load(Ordering::Relaxed)
    }

    /// Stops the adapter: closes the link, waits up to [`STOP_GRACE`] for a spawned child
    /// and kills it. Resolves once the state is `Off`.
    pub fn shutdown(&self) -> impl Future<Output = ()> + Send + 'static {
        self.stop.send_replace(true);
        let mut state = self.state.clone();
        async move {
            let _ = state.wait_for(|state| *state == AdapterState::Off).await;
        }
    }
}

/// Decides which failures are logged as warnings: a new reason, or the same one after
/// [`REPEAT_LOG_INTERVAL`].
#[derive(Debug, Default)]
struct FailureLog {
    last: Option<(String, Instant)>,
}

impl FailureLog {
    fn should_warn(&mut self, reason: &str, now: Instant) -> bool {
        if let Some((last, at)) = &self.last {
            if last == reason && now.saturating_duration_since(*at) < REPEAT_LOG_INTERVAL {
                return false;
            }
        }
        self.last = Some((reason.to_owned(), now));
        true
    }
}

struct Runner {
    starter: Arc<dyn Starter>,
    hello: BridgeHello,
    state: watch::Sender<AdapterState>,
    current: watch::Sender<Option<ReadyLink>>,
    restarts: Arc<AtomicU64>,
    stop: watch::Receiver<bool>,
    observer: Option<mpsc::UnboundedSender<(Instant, AdapterState)>>,
    backoff: Backoff,
    failures: FailureLog,
    next_id: u64,
}

/// The supervisor was told to stop (or every handle is gone).
struct Stopped;

impl Runner {
    async fn run(mut self) {
        let _ = self.supervise().await;
        self.current.send_replace(None);
        self.set(AdapterState::Off);
    }

    async fn supervise(&mut self) -> Result<(), Stopped> {
        let mut first = true;
        loop {
            if !first {
                self.restarts.fetch_add(1, Ordering::Relaxed);
            }
            first = false;
            let until = self.attempt().await?;
            let mut stop = self.stop.clone();
            tokio::select! {
                () = sleep_until(until) => {}
                () = stopped(&mut stop) => return Err(Stopped),
            }
        }
    }

    /// Publishes the backoff after a failure: when the next start is due.
    fn back_off(&mut self, reason: String) -> Instant {
        let delay = self.backoff.next_delay();
        self.log_failure(&reason, delay);
        let until = Instant::now() + delay;
        self.set(AdapterState::Backoff { until, reason });
        until
    }

    /// One start: returns when the next one is due. The backoff is published before a
    /// spawned child is waited for, so the adapter is never `Ready` while it stops; the
    /// backoff counts from the failure.
    async fn attempt(&mut self) -> Result<Instant, Stopped> {
        let mut stop = self.stop.clone();
        if *stop.borrow() {
            return Err(Stopped);
        }
        self.set(AdapterState::Starting);
        let started = tokio::select! {
            started = self.starter.start() => started,
            () = stopped(&mut stop) => return Err(Stopped),
        };
        let Started {
            reader,
            writer,
            child,
        } = match started {
            Ok(started) => started,
            Err(error) => return Ok(self.back_off(error.to_string())),
        };
        self.set(AdapterState::Handshaking);
        let opened = tokio::select! {
            opened = Link::open(reader, writer, self.hello.clone()) => Some(opened),
            () = stopped(&mut stop) => None,
        };
        let outcome = match opened {
            None => Err(Stopped),
            Some(Err(end)) => Ok(end.to_string()),
            Some(Ok((link, events))) => self.ready(link, events).await,
        };
        let outcome = outcome.map(|reason| self.back_off(reason));
        if let Some(child) = child {
            child.finish(STOP_GRACE).await;
        }
        outcome
    }

    /// Publishes the link and waits for its end.
    async fn ready(
        &mut self,
        link: Link,
        events: mpsc::UnboundedReceiver<LinkEvent>,
    ) -> Result<String, Stopped> {
        let since = Instant::now();
        self.next_id += 1;
        let ready = ReadyLink {
            id: self.next_id,
            link: link.clone(),
            events: Arc::new(Mutex::new(Some(events))),
            returned: Arc::default(),
        };
        let slot = Arc::clone(&ready.events);
        self.set(AdapterState::Ready(link.info().clone()));
        self.current.send_replace(Some(ready));
        let mut closed = link.closed();
        let mut stop = self.stop.clone();
        let mut drain_tick = interval(DRAIN_INTERVAL);
        drain_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let end = loop {
            tokio::select! {
                end = closed.wait() => break Some(end),
                () = stopped(&mut stop) => break None,
                _ = drain_tick.tick() => drain(&slot),
            }
        };
        self.current.send_replace(None);
        link.close();
        drop(link);
        if Instant::now().saturating_duration_since(since) >= RESET_AFTER {
            self.backoff.reset();
        }
        match end {
            Some(end) => Ok(end.to_string()),
            None => Err(Stopped),
        }
    }

    fn set(&self, state: AdapterState) {
        if let Some(observer) = &self.observer {
            let _ = observer.send((Instant::now(), state.clone()));
        }
        self.state.send_replace(state);
    }

    fn log_failure(&mut self, reason: &str, retry: Duration) {
        let retry_in_s = retry.as_secs();
        if self.failures.should_warn(reason, Instant::now()) {
            tracing::warn!(event = "voice_adapter_failed", reason = %reason, retry_in_s);
        } else {
            tracing::debug!(event = "voice_adapter_failed", reason = %reason, retry_in_s);
        }
    }
}

/// Resolves when the supervisor must stop: told to, or every handle dropped.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

#[cfg(test)]
pub(crate) mod testing {
    //! A [`Starter`] running the testkit's scripted adapter in-process.

    use std::collections::VecDeque;

    use push_voice_adapter_testkit::{InProcess, Log, Script, duplex};
    use tokio::task::JoinHandle;

    use super::*;

    /// What the next start does.
    pub(crate) enum Plan {
        Adapter(Script),
        Fail(io::ErrorKind),
    }

    type Serving = JoinHandle<
        Result<push_voice_adapter_testkit::Outcome, push_voice_adapter_testkit::AdapterError>,
    >;

    /// Starts the planned adapters in order, then default ones.
    pub(crate) struct ScriptedStarter {
        plans: Mutex<VecDeque<Plan>>,
        serving: Mutex<Vec<Serving>>,
        logs: Mutex<Vec<Log>>,
    }

    impl ScriptedStarter {
        pub(crate) fn new(plans: impl IntoIterator<Item = Plan>) -> Arc<Self> {
            Arc::new(Self {
                plans: Mutex::new(plans.into_iter().collect()),
                serving: Mutex::default(),
                logs: Mutex::default(),
            })
        }

        /// Ends every adapter running, without a proper close (as a crash).
        pub(crate) fn crash(&self) {
            for task in lock(&self.serving).drain(..) {
                task.abort();
            }
        }

        /// The adapters' tasks, oldest first (taken).
        pub(crate) fn take_serving(&self) -> Vec<Serving> {
            lock(&self.serving).drain(..).collect()
        }

        pub(crate) fn logs(&self) -> Vec<Log> {
            lock(&self.logs).clone()
        }
    }

    #[async_trait]
    impl Starter for ScriptedStarter {
        async fn start(&self) -> io::Result<Started> {
            let plan = lock(&self.plans)
                .pop_front()
                .unwrap_or(Plan::Adapter(Script::default()));
            let script = match plan {
                Plan::Adapter(script) => script,
                Plan::Fail(kind) => return Err(io::Error::new(kind, format!("planned: {kind}"))),
            };
            let InProcess {
                reader,
                writer,
                log,
                task,
            } = duplex(script);
            lock(&self.serving).push(task);
            lock(&self.logs).push(log);
            Ok(Started {
                reader: Box::new(reader),
                writer: Box::new(writer),
                child: None,
            })
        }
    }

    /// A supervisor of `starter` reporting every state it enters, with when.
    pub(crate) fn observed(
        starter: Arc<dyn Starter>,
    ) -> (Supervisor, mpsc::UnboundedReceiver<(Instant, AdapterState)>) {
        let (observer, states) = mpsc::unbounded_channel();
        let supervisor =
            Supervisor::launch(starter, BridgeHello::v1("1.2.3", "en-US"), Some(observer));
        (supervisor, states)
    }
}

#[cfg(test)]
mod tests {
    use push_voice_adapter_testkit::Script;
    use push_voice_adapter_testkit::script::DEFAULT_NAME;
    use tokio::time::sleep;

    use super::testing::{Plan, ScriptedStarter, observed};
    use super::*;
    use crate::config::StderrMode;
    use crate::voice::adapter::link::LinkEnd;

    type States = mpsc::UnboundedReceiver<(Instant, AdapterState)>;

    async fn next(states: &mut States) -> (Instant, AdapterState) {
        timeout(Duration::from_secs(600), states.recv())
            .await
            .expect("a state change")
            .expect("the supervisor runs")
    }

    async fn expect_ready(states: &mut States) -> AdapterInfo {
        assert_eq!(next(states).await.1, AdapterState::Starting);
        assert_eq!(next(states).await.1, AdapterState::Handshaking);
        match next(states).await.1 {
            AdapterState::Ready(info) => info,
            other => panic!("expected ready, got {other:?}"),
        }
    }

    /// The next backoff's delay and reason.
    async fn expect_backoff(states: &mut States) -> (Duration, String) {
        match next(states).await {
            (at, AdapterState::Backoff { until, reason }) => (until - at, reason),
            (_, other) => panic!("expected a backoff, got {other:?}"),
        }
    }

    /// A start that fails, skipping its `Starting`.
    async fn expect_failed_start(states: &mut States) -> (Duration, String) {
        assert_eq!(next(states).await.1, AdapterState::Starting);
        expect_backoff(states).await
    }

    #[tokio::test(start_paused = true)]
    async fn starts_handshakes_and_publishes_the_link() {
        let starter = ScriptedStarter::new([]);
        let (supervisor, mut states) = observed(starter);
        let info = expect_ready(&mut states).await;
        assert_eq!(info.name, DEFAULT_NAME);
        assert!(info.capabilities.stt);
        assert_eq!(supervisor.state().borrow().ready(), Some(&info));
        let current = supervisor.current().borrow().clone().expect("a ready link");
        assert_eq!(current.link().info(), &info);
        assert!(current.take_events().is_some());
        assert!(current.take_events().is_none(), "one consumer");
        assert_eq!(supervisor.restarts(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_adapter_that_goes_away_is_restarted_after_a_second() {
        let starter = ScriptedStarter::new([]);
        let (supervisor, mut states) = observed(starter.clone());
        expect_ready(&mut states).await;
        let first = supervisor.current().borrow().clone().unwrap();
        starter.crash();
        let (backoff_at, state) = next(&mut states).await;
        let AdapterState::Backoff { until, reason } = state else {
            panic!("expected a backoff, got {state:?}");
        };
        assert_eq!(until - backoff_at, Duration::from_secs(1));
        assert_eq!(reason, LinkEnd::Eof.to_string());
        assert!(supervisor.current().borrow().is_none());
        assert_eq!(first.link().end(), Some(LinkEnd::Eof));
        let (at, state) = next(&mut states).await;
        assert_eq!(state, AdapterState::Starting);
        assert_eq!(at, until);
        assert_eq!(next(&mut states).await.1, AdapterState::Handshaking);
        assert!(matches!(next(&mut states).await.1, AdapterState::Ready(_)));
        let second = supervisor.current().borrow().clone().unwrap();
        assert!(second.id() > first.id());
        assert_eq!(supervisor.restarts(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn quick_failures_back_off_1_2_4_seconds() {
        let starter = ScriptedStarter::new([
            Plan::Fail(io::ErrorKind::NotFound),
            Plan::Fail(io::ErrorKind::NotFound),
            Plan::Adapter(Script {
                refuse_format: true,
                ..Script::default()
            }),
        ]);
        let (supervisor, mut states) = observed(starter);
        let mut delays = Vec::new();
        for _ in 0..2 {
            delays.push(expect_failed_start(&mut states).await.0);
        }
        assert_eq!(next(&mut states).await.1, AdapterState::Starting);
        assert_eq!(next(&mut states).await.1, AdapterState::Handshaking);
        delays.push(expect_backoff(&mut states).await.0);
        assert_eq!(delays, [1, 2, 4].map(Duration::from_secs));
        expect_ready(&mut states).await;
        assert_eq!(supervisor.restarts(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_minute_ready_starts_the_backoff_over() {
        let starter = ScriptedStarter::new([Plan::Fail(io::ErrorKind::NotFound)]);
        let (_supervisor, mut states) = observed(starter.clone());
        assert_eq!(
            expect_failed_start(&mut states).await.0,
            Duration::from_secs(1)
        );
        expect_ready(&mut states).await;
        starter.crash();
        assert_eq!(expect_backoff(&mut states).await.0, Duration::from_secs(2));
        expect_ready(&mut states).await;
        sleep(RESET_AFTER + Duration::from_secs(1)).await;
        starter.crash();
        assert_eq!(expect_backoff(&mut states).await.0, Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_format_backs_off_naming_the_code() {
        let starter = ScriptedStarter::new([Plan::Adapter(Script {
            refuse_format: true,
            ..Script::default()
        })]);
        let (supervisor, mut states) = observed(starter);
        assert_eq!(next(&mut states).await.1, AdapterState::Starting);
        assert_eq!(next(&mut states).await.1, AdapterState::Handshaking);
        let (delay, reason) = expect_backoff(&mut states).await;
        assert_eq!(delay, Duration::from_secs(1));
        assert!(reason.contains("unsupported_format"), "{reason}");
        assert!(supervisor.current().borrow().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_error_is_the_reason() {
        let starter = ScriptedStarter::new([Plan::Fail(io::ErrorKind::PermissionDenied)]);
        let (_supervisor, mut states) = observed(starter);
        let (_, reason) = expect_failed_start(&mut states).await;
        assert_eq!(reason, "planned: permission denied");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_closes_the_link_and_ends_off() {
        let starter = ScriptedStarter::new([]);
        let (supervisor, mut states) = observed(starter.clone());
        expect_ready(&mut states).await;
        let current = supervisor.current().borrow().clone().unwrap();
        timeout(Duration::from_secs(10), supervisor.shutdown())
            .await
            .expect("shutdown finishes");
        assert_eq!(*supervisor.state().borrow(), AdapterState::Off);
        assert!(supervisor.current().borrow().is_none());
        assert_eq!(current.link().end(), Some(LinkEnd::Shutdown));
        for task in starter.take_serving() {
            let outcome = timeout(Duration::from_secs(10), task).await.unwrap();
            assert_eq!(
                outcome.unwrap().unwrap(),
                push_voice_adapter_testkit::Outcome::Closed
            );
        }
        assert_eq!(starter.logs().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_backoff_ends_off_at_once() {
        let starter = ScriptedStarter::new([Plan::Fail(io::ErrorKind::NotFound)]);
        let (supervisor, mut states) = observed(starter);
        expect_failed_start(&mut states).await;
        let begun = Instant::now();
        supervisor.shutdown().await;
        assert!(Instant::now() - begun < Duration::from_secs(1));
        assert_eq!(next(&mut states).await.1, AdapterState::Off);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_every_handle_stops_the_adapter() {
        let starter = ScriptedStarter::new([]);
        let (supervisor, mut states) = observed(starter);
        expect_ready(&mut states).await;
        drop(supervisor);
        assert_eq!(next(&mut states).await.1, AdapterState::Off);
    }

    #[tokio::test]
    async fn off_is_off() {
        let supervisor = Supervisor::off();
        assert_eq!(*supervisor.state().borrow(), AdapterState::Off);
        assert!(supervisor.current().borrow().is_none());
        timeout(Duration::from_secs(1), supervisor.shutdown())
            .await
            .unwrap();
        assert_eq!(supervisor.restarts(), 0);
    }

    #[tokio::test]
    async fn untaken_events_are_drained() {
        let (events, receiver) = mpsc::unbounded_channel();
        let slot = Mutex::new(Some(receiver));
        for _ in 0..3 {
            events.send(LinkEvent::Closed(LinkEnd::Eof)).unwrap();
        }
        drain(&slot);
        assert!(lock(&slot).as_mut().unwrap().try_recv().is_err());
        drain(&Mutex::new(None));
    }

    #[test]
    fn repeated_reasons_warn_once_a_minute() {
        let mut log = FailureLog::default();
        let start = Instant::now();
        assert!(log.should_warn("a", start));
        assert!(!log.should_warn("a", start + Duration::from_secs(30)));
        assert!(log.should_warn("b", start + Duration::from_secs(31)));
        assert!(log.should_warn("a", start + Duration::from_secs(32)));
        assert!(!log.should_warn("a", start + Duration::from_secs(91)));
        assert!(log.should_warn("a", start + Duration::from_secs(93)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_program_backs_off_naming_its_file() {
        let dir = crate::test_dir::TestDir::new();
        let program = dir.path().join("nowhere").join("voice-adapter.exe");
        let config = AdapterConfig::Spawn(SpawnConfig {
            command: vec![program.to_string_lossy().into_owned()],
            stderr: StderrMode::Discard,
            env_allow: Vec::new(),
        });
        let supervisor = start(
            config,
            BridgeHello::v1("1.2.3", "en-US"),
            dir.path().to_owned(),
            Vec::new(),
        );
        let mut state = supervisor.state();
        let backoff = timeout(
            Duration::from_secs(10),
            state.wait_for(|state| matches!(state, AdapterState::Backoff { .. })),
        )
        .await
        .unwrap()
        .unwrap()
        .clone();
        let AdapterState::Backoff { reason, .. } = backoff else {
            unreachable!()
        };
        assert_eq!(
            reason,
            "voice adapter program voice-adapter.exe was not found"
        );
        supervisor.shutdown().await;
    }

    /// A unique local endpoint and the config that names it; the guard keeps a socket's
    /// folder.
    fn scratch() -> (
        ConnectConfig,
        push_bridge_control::Endpoint,
        Option<crate::test_dir::TestDir>,
    ) {
        let name = format!("push-voice-supervisor-{}", uuid::Uuid::new_v4());
        if cfg!(windows) {
            let config = ConnectConfig::Pipe(name);
            let endpoint = super::super::connect::endpoint(&config).unwrap();
            (config, endpoint, None)
        } else {
            let dir = crate::test_dir::TestDir::new();
            let path = dir.path().join("adapter").join("voice.sock");
            let config = ConnectConfig::Socket(path.to_string_lossy().into_owned());
            (
                config,
                push_bridge_control::Endpoint::Socket(path),
                Some(dir),
            )
        }
    }

    #[tokio::test]
    async fn connects_to_a_real_endpoint() {
        let (config, endpoint, _dir) = scratch();
        let mut listener = push_bridge_control::transport::Listener::bind(&endpoint).unwrap();
        let adapter = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (reader, writer) = tokio::io::split(stream);
            push_voice_adapter_testkit::ScriptedAdapter::run(reader, writer, Script::default())
                .await
        });
        let dir = crate::test_dir::TestDir::new();
        let supervisor = start(
            AdapterConfig::Connect(config),
            BridgeHello::v1("1.2.3", "en-US"),
            dir.path().to_owned(),
            Vec::new(),
        );
        let mut state = supervisor.state();
        timeout(
            Duration::from_secs(20),
            state.wait_for(|state| state.ready().is_some()),
        )
        .await
        .expect("ready")
        .unwrap();
        timeout(Duration::from_secs(20), supervisor.shutdown())
            .await
            .unwrap();
        let outcome = timeout(Duration::from_secs(20), adapter).await.unwrap();
        assert_eq!(
            outcome.unwrap().unwrap(),
            push_voice_adapter_testkit::Outcome::Closed
        );
    }

    const SLEEPER: &str = "voice::adapter::supervisor::tests::sleep_a_while";
    const MARKER: &str = "voice-supervisor-sleeper-marker";

    /// Runs in a child process: ignores its stdin and sleeps.
    #[test]
    #[ignore = "a helper the supervisor tests run in a child process"]
    fn sleep_a_while() {
        if std::env::args().any(|arg| arg == MARKER) {
            std::thread::sleep(Duration::from_secs(120));
        }
    }

    #[tokio::test]
    async fn a_child_that_does_not_exit_is_killed_after_the_grace() {
        let dir = crate::test_dir::TestDir::new();
        let exe = std::env::current_exe().unwrap();
        let mut command = vec![exe.to_string_lossy().into_owned()];
        command.extend(
            [SLEEPER, "--exact", "--ignored", "--test-threads=1", MARKER].map(str::to_owned),
        );
        let starter = SpawnStarter {
            config: SpawnConfig {
                command,
                stderr: StderrMode::Discard,
                env_allow: Vec::new(),
            },
            log_dir: dir.path().to_owned(),
            extra_secret_names: Vec::new(),
        };
        let started = starter.start().await.unwrap();
        drop(started.writer);
        let child = started.child.expect("a child");
        let begun = std::time::Instant::now();
        timeout(
            Duration::from_secs(30),
            child.finish(Duration::from_millis(500)),
        )
        .await
        .expect("the child is killed");
        assert!(begun.elapsed() < Duration::from_secs(30));
    }

    fn sleeper_config() -> SpawnConfig {
        let exe = std::env::current_exe().unwrap();
        let mut command = vec![exe.to_string_lossy().into_owned()];
        command.extend(
            [SLEEPER, "--exact", "--ignored", "--test-threads=1", MARKER].map(str::to_owned),
        );
        SpawnConfig {
            command,
            stderr: StderrMode::Discard,
            env_allow: Vec::new(),
        }
    }

    /// The testkit's adapter over pipes, with a spawned child that does not exit by
    /// itself (as an adapter slow to stop).
    struct SlowChildStarter {
        adapters: Arc<ScriptedStarter>,
        config: SpawnConfig,
        log_dir: PathBuf,
    }

    #[async_trait]
    impl Starter for SlowChildStarter {
        async fn start(&self) -> io::Result<Started> {
            let mut started = self.adapters.start().await?;
            let spawned = spawn(&self.config, &self.log_dir, &[])?;
            let (stdin, mut stdout) = (spawned.stdin, spawned.stdout);
            tokio::spawn(async move {
                let _stdin = stdin;
                let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
            });
            started.child = Some(ChildGuard::new(spawned.child, spawned.job));
            Ok(started)
        }
    }

    #[tokio::test]
    async fn a_slow_child_is_not_ready_while_it_stops_and_backs_off_meanwhile() {
        let dir = crate::test_dir::TestDir::new();
        let adapters = ScriptedStarter::new([]);
        let starter = Arc::new(SlowChildStarter {
            adapters: adapters.clone(),
            config: sleeper_config(),
            log_dir: dir.path().to_owned(),
        });
        let (supervisor, mut states) = observed(starter);
        expect_ready(&mut states).await;
        let crashed = Instant::now();
        adapters.crash();
        let (at, state) = next(&mut states).await;
        let AdapterState::Backoff { until, .. } = state else {
            panic!("expected a backoff, got {state:?}");
        };
        assert!(
            at - crashed < Duration::from_secs(1),
            "backoff at once, not after the child's grace"
        );
        let delay = until - at;
        assert!(
            delay <= Duration::from_secs(1) && delay > Duration::from_millis(900),
            "{delay:?}"
        );
        let (started, state) = next(&mut states).await;
        assert_eq!(state, AdapterState::Starting);
        assert!(started >= until);
        assert!(started - crashed >= STOP_GRACE, "after the child stopped");
        assert!(
            started - crashed < STOP_GRACE + Duration::from_millis(900),
            "the backoff only waits for what is left: {:?}",
            started - crashed
        );
        timeout(Duration::from_secs(30), supervisor.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn events_given_back_can_be_waited_for() {
        let starter = ScriptedStarter::new([]);
        let (supervisor, mut states) = observed(starter);
        expect_ready(&mut states).await;
        let current = supervisor.current().borrow().clone().unwrap();
        let events = current.take_events().unwrap();
        let begun = Instant::now();
        assert!(
            current
                .wait_for_events(Duration::from_millis(500))
                .await
                .is_none()
        );
        assert!(Instant::now() - begun >= Duration::from_millis(500));
        let giver = current.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            giver.give_back(events);
        });
        let begun = Instant::now();
        assert!(
            current
                .wait_for_events(Duration::from_secs(2))
                .await
                .is_some()
        );
        assert_eq!(Instant::now() - begun, Duration::from_millis(300));
    }
}
