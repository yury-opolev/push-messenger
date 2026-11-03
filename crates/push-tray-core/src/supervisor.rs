//! Supervises the bridge instances: starts them (never one that already runs, under the
//! tray or not), restarts them after a failure, stops them, keeps a control channel to
//! each, and publishes what the tray shows.
//!
//! The [`Supervisor`] is an actor: [`Supervisor::run`] takes [`Command`]s and ticks every
//! [`TICK`]; it publishes the [`InstanceView`]s through a `watch` channel and the
//! [`Notice`]s worth a notification through an `mpsc` channel. Processes, connections and
//! Scheduled Tasks come through [`Spawner`], [`Connector`] and [`Scheduler`], so tests
//! run with fakes and paused time.
//!
//! Bridges the tray started are not tied to the tray's life: when the tray dies they keep
//! running (and the next tray finds them reachable and shows them as external). Quit
//! stops them gracefully with `shutdown`.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use push_bridge_control::{
    BridgeState, Call, ErrorBody, Event, EventLine, PauseParams, PeekEntry, Status, instance_id,
    now_ms,
};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::client::{self, ControlClient};
use crate::history::History;
use crate::menu::AgentAction;
use crate::policy::{Decision, RestartPolicy};
use crate::schedtask::{Scheduler, TaskFor, TaskId};
use crate::state::{AgentState, from_bridge, label};
use crate::tray_config::TrayConfig;

/// How often the supervisor looks at its processes and timers.
pub const TICK: Duration = Duration::from_millis(500);
/// The pause between connection attempts to a bridge that is starting.
pub const CONNECT_RETRY: Duration = Duration::from_millis(500);
/// How long a starting bridge is retried every [`CONNECT_RETRY`].
pub const CONNECT_PATIENCE: Duration = Duration::from_secs(30);
/// How often a bridge that is not reachable is tried again (and `status` re-fetched).
pub const PROBE_EVERY: Duration = Duration::from_secs(10);
/// How long `shutdown` may take to answer.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long Stop tries to open a control channel to a bridge the tray started but is not
/// connected to, to ask it to shut down before killing it.
pub const STOP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a switch-over waits for a Scheduled Task's bridge to go away (after its task
/// was stopped and `shutdown` sent) before giving up rather than starting a second one.
pub const SWITCH_PATIENCE: Duration = Duration::from_secs(20);
/// The history entry for a bridge that exited with code 0 without the tray asking.
pub const STOPPED_ELSEWHERE: &str = "stopped outside the tray (exit 0)";
/// How long a bridge has to exit after `shutdown` before it is killed.
pub const EXIT_GRACE: Duration = Duration::from_secs(20);
/// How long `restart_backend`, `reconnect` and `resume_backend` may take (the bridge's
/// own limit is 30 s).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(35);
/// How long `pause_backend` (with the default drain) may take (the bridge's own limit is
/// the drain plus 2 min: 12 min).
pub const PAUSE_TIMEOUT: Duration = Duration::from_secs(15 * 60 + 30);
/// How long a `peek` may take.
pub const PEEK_TIMEOUT: Duration = Duration::from_secs(5);
/// Notices waiting for the shell.
const NOTICE_BUFFER: usize = 32;

/// A boxed future, for the object-safe [`Connector`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// A connected control channel and its events.
pub type Connection = (ControlClient, mpsc::Receiver<EventLine>);

/// How a process ended: its exit code, `None` when a signal ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
}

/// A bridge process the tray started.
pub trait Child: Send {
    fn id(&self) -> Option<u32>;
    /// `Some` once it has exited.
    fn try_wait(&mut self) -> io::Result<Option<Exit>>;
    fn kill(&mut self) -> io::Result<()>;
}

/// Starts bridge processes.
pub trait Spawner: Send + Sync {
    /// Starts `push-agent-bridge --dir <dir> run`.
    fn spawn(&self, dir: &Path) -> io::Result<Box<dyn Child>>;
}

/// Opens a bridge's control channel.
pub trait Connector: Send + Sync {
    /// Nobody listening is [`io::ErrorKind::NotFound`] (or `ConnectionRefused`).
    fn connect<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, io::Result<Connection>>;
}

/// The real control channel: [`client::connect`].
#[derive(Debug, Clone, Copy, Default)]
pub struct EndpointConnector;

impl Connector for EndpointConnector {
    fn connect<'a>(&'a self, dir: &'a Path) -> BoxFuture<'a, io::Result<Connection>> {
        Box::pin(client::connect(dir))
    }
}

/// Starts `<bridge_exe> --dir <dir> run` with no window (`CREATE_NO_WINDOW` on Windows)
/// and stdin, stdout and stderr null (the bridge logs to its folder). The process is not
/// killed when the tray drops it or exits.
#[derive(Debug, Clone)]
pub struct ProcessSpawner {
    pub bridge_exe: PathBuf,
}

impl Spawner for ProcessSpawner {
    fn spawn(&self, dir: &Path) -> io::Result<Box<dyn Child>> {
        let child = crate::hidden_command(&self.bridge_exe)
            .arg("--dir")
            .arg(dir)
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Box::new(ProcessChild(child)))
    }
}

struct ProcessChild(std::process::Child);

impl Child for ProcessChild {
    fn id(&self) -> Option<u32> {
        Some(self.0.id())
    }

    fn try_wait(&mut self) -> io::Result<Option<Exit>> {
        Ok(self.0.try_wait()?.map(|status| Exit {
            code: status.code(),
        }))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }
}

/// `push-agent-bridge(.exe)` in the tray's folder.
pub fn bridge_exe_next_to(tray_exe: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "push-agent-bridge.exe"
    } else {
        "push-agent-bridge"
    };
    tray_exe.with_file_name(name)
}

/// A configured instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    pub dir: PathBuf,
    /// Start it when the tray starts.
    pub autostart: bool,
    /// The Scheduled Task that runs it: the tray never spawns it, Start runs the task.
    pub task: Option<TaskId>,
}

/// The instances of `config`, each with the Scheduled Task that runs it (from
/// [`crate::schedtask::tasks_for_instances`]).
pub fn specs(config: &TrayConfig, found: &[TaskFor]) -> Vec<InstanceSpec> {
    config
        .instances
        .iter()
        .map(|instance| {
            let id = instance_id(&instance.dir);
            InstanceSpec {
                dir: instance.dir.clone(),
                autostart: instance.autostart,
                task: found
                    .iter()
                    .find(|item| instance_id(&item.dir) == id)
                    .map(TaskFor::id),
            }
        })
        .collect()
}

/// An instance as the tray shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceView {
    pub dir: PathBuf,
    /// The instance id ([`push_bridge_control::instance_id`]).
    pub id: String,
    /// "<display name> (<handle>)" once known, else the folder's name.
    pub name: String,
    pub state: AgentState,
    /// The tray started the running process (Quit stops only these).
    pub started_by_tray: bool,
    /// The control channel is open.
    pub connected: bool,
    pub autostart: bool,
    pub task: Option<TaskId>,
    /// The last `status` (kept after the connection is gone).
    pub status: Option<Status>,
    pub history: History,
    pub last_exit: Option<i32>,
    pub last_error: Option<String>,
    /// How many `peek_changed` events the bridge sent (the window peeks again when it
    /// grows).
    pub peek_changes: u64,
    /// The open control channel, for the window's `peek`.
    pub control: Option<ControlLink>,
    /// The last action the tray forwarded that the bridge refused or that could not be
    /// sent; cleared by the next action.
    pub refusal: Option<Refusal>,
}

/// An action the bridge refused (or that could not be sent), shown next to its button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub action: AgentAction,
    /// "<message> (<code>)", or "not connected".
    pub message: String,
    /// Unix milliseconds.
    pub at: i64,
}

/// A link to an instance's open control channel; equal when it is the same channel.
#[derive(Clone)]
pub struct ControlLink(Arc<ControlClient>);

impl ControlLink {
    /// The bridge's last `limit` peek entries, oldest first (at most [`PEEK_TIMEOUT`]).
    pub async fn peek(&self, limit: usize) -> Result<Vec<PeekEntry>, ErrorBody> {
        self.0.peek(limit, PEEK_TIMEOUT).await
    }
}

impl PartialEq for ControlLink {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for ControlLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlLink")
    }
}

/// What the supervisor is asked to do. Instances are named by folder.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// The configured instances: new ones are added (autostart ones started), missing ones
    /// stopped (when the tray runs them) and removed.
    SetInstances(Vec<InstanceSpec>),
    Start(PathBuf),
    Stop(PathBuf),
    RestartBackend(PathBuf),
    Reconnect(PathBuf),
    Pause(PathBuf),
    Resume(PathBuf),
    /// Re-fetch every status and look for bridges now.
    Refresh,
    /// Stop the instances the tray started, leave the others, and return from `run`.
    Quit,
}

impl Command {
    /// The folder of an agent action (Start, Stop, Restart Coda, Reconnect, Pause, Resume).
    fn action_dir(&self) -> Option<&Path> {
        match self {
            Command::Start(dir)
            | Command::Stop(dir)
            | Command::RestartBackend(dir)
            | Command::Reconnect(dir)
            | Command::Pause(dir)
            | Command::Resume(dir) => Some(dir),
            Command::SetInstances(_) | Command::Refresh | Command::Quit => None,
        }
    }
}

/// Something worth a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    GaveUp {
        dir: PathBuf,
        name: String,
        code: Option<i32>,
    },
    NotSetUp {
        dir: PathBuf,
        name: String,
    },
}

/// What a connection attempt is for.
#[derive(Debug, Clone, Copy)]
enum Purpose {
    /// Start: reachable → external; nobody listening → spawn.
    Start,
    /// Look for the bridge: every [`CONNECT_RETRY`] until `fast_until`, then every
    /// [`PROBE_EVERY`].
    Retry { fast_until: Instant },
}

struct Instance {
    dir: PathBuf,
    id: String,
    autostart: bool,
    task: Option<TaskId>,
    state: AgentState,
    /// The last state the bridge reported.
    reported: Option<AgentState>,
    history: History,
    policy: RestartPolicy,
    child: Option<Box<dyn Child>>,
    spawned_at: Instant,
    client: Option<Arc<ControlClient>>,
    /// The current connection's number.
    conn: u64,
    /// The connection attempt in flight.
    attempt: Option<u64>,
    purpose: Purpose,
    next_connect: Option<Instant>,
    restart_at: Option<Instant>,
    requested_stop: bool,
    kill_at: Option<Instant>,
    status: Option<Status>,
    status_pending: bool,
    next_status: Option<Instant>,
    task_running: bool,
    last_exit: Option<i32>,
    last_error: Option<String>,
    peek_changes: u64,
    refusal: Option<Refusal>,
    removed: bool,
}

impl Instance {
    fn new(spec: InstanceSpec, id: String, now: Instant) -> Self {
        Self {
            dir: spec.dir,
            id,
            autostart: spec.autostart,
            task: spec.task,
            state: AgentState::Stopped,
            reported: None,
            history: History::new(),
            policy: RestartPolicy::new(),
            child: None,
            spawned_at: now,
            client: None,
            conn: 0,
            attempt: None,
            purpose: Purpose::Retry { fast_until: now },
            next_connect: Some(now),
            restart_at: None,
            requested_stop: false,
            kill_at: None,
            status: None,
            status_pending: false,
            next_status: None,
            task_running: false,
            last_exit: None,
            last_error: None,
            peek_changes: 0,
            refusal: None,
            removed: false,
        }
    }

    fn set_state(&mut self, state: AgentState) {
        if self.state != state {
            self.history.push(now_ms(), label(&state));
            self.state = state;
        }
    }

    /// The state of a reachable bridge the tray did not start.
    fn external(&self) -> AgentState {
        AgentState::External {
            task: self.task.is_some(),
        }
    }

    /// The state the bridge reports, as the tray shows it: an idle bridge the tray did
    /// not start is external.
    fn shown(&self, state: BridgeState, detail: Option<String>) -> AgentState {
        let state = from_bridge(state, detail);
        if self.child.is_none() && state == AgentState::Idle {
            return self.external();
        }
        state
    }

    /// Takes the state the bridge reported; shown unless the tray asked it to stop.
    fn report(&mut self, state: AgentState) {
        self.reported = Some(state.clone());
        if !self.requested_stop {
            self.set_state(state);
        }
    }

    fn name(&self) -> String {
        if let Some(agent) = self
            .status
            .as_ref()
            .and_then(|status| status.agent.as_ref())
        {
            return format!("{} ({})", agent.display_name, agent.handle);
        }
        self.dir.file_name().map_or_else(
            || self.dir.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    fn view(&self) -> InstanceView {
        InstanceView {
            dir: self.dir.clone(),
            id: self.id.clone(),
            name: self.name(),
            state: self.state.clone(),
            started_by_tray: self.child.is_some(),
            connected: self.client.is_some(),
            autostart: self.autostart,
            task: self.task.clone(),
            status: self.status.clone(),
            history: self.history.clone(),
            last_exit: self.last_exit,
            last_error: self.last_error.clone(),
            peek_changes: self.peek_changes,
            control: self
                .client
                .as_ref()
                .map(|client| ControlLink(Arc::clone(client))),
            refusal: self.refusal.clone(),
        }
    }

    /// Looks for the bridge from `now` on, quickly until `fast_until`.
    fn retry(&mut self, now: Instant, fast_until: Instant, first: Duration) {
        self.attempt = None;
        self.purpose = Purpose::Retry { fast_until };
        self.next_connect = Some(now + first);
    }
}

/// What the supervisor's own tasks report back.
enum Internal {
    Connected {
        id: String,
        attempt: u64,
        result: io::Result<Connection>,
    },
    Event {
        id: String,
        conn: u64,
        line: EventLine,
    },
    Disconnected {
        id: String,
        conn: u64,
    },
    Status {
        id: String,
        conn: u64,
        result: Box<Result<Status, ErrorBody>>,
    },
    Called {
        id: String,
        method: &'static str,
        action: AgentAction,
        result: Result<Value, ErrorBody>,
    },
    ShutdownDone {
        id: String,
        result: Result<Value, ErrorBody>,
        /// Sent over a connection opened for it (Stop while not connected).
        fresh: bool,
    },
    TaskRun {
        id: String,
        result: io::Result<()>,
    },
}

/// Supervises the instances; see the module docs.
pub struct Supervisor {
    spawner: Arc<dyn Spawner>,
    connector: Arc<dyn Connector>,
    scheduler: Arc<dyn Scheduler + Send + Sync>,
    instances: Vec<Instance>,
    views: watch::Sender<Vec<InstanceView>>,
    notices: mpsc::Sender<Notice>,
    inbox: mpsc::UnboundedSender<Internal>,
    inbox_receiver: Option<mpsc::UnboundedReceiver<Internal>>,
    seq: u64,
    quitting: bool,
}

impl Supervisor {
    /// A supervisor without instances (send [`Command::SetInstances`] or call
    /// [`Supervisor::set_instances`]), the views it publishes and its notices.
    pub fn new(
        spawner: Arc<dyn Spawner>,
        connector: Arc<dyn Connector>,
        scheduler: Arc<dyn Scheduler + Send + Sync>,
    ) -> (
        Self,
        watch::Receiver<Vec<InstanceView>>,
        mpsc::Receiver<Notice>,
    ) {
        let (views, view_receiver) = watch::channel(Vec::new());
        let (notices, notice_receiver) = mpsc::channel(NOTICE_BUFFER);
        let (inbox, inbox_receiver) = mpsc::unbounded_channel();
        (
            Self {
                spawner,
                connector,
                scheduler,
                instances: Vec::new(),
                views,
                notices,
                inbox,
                inbox_receiver: Some(inbox_receiver),
                seq: 0,
                quitting: false,
            },
            view_receiver,
            notice_receiver,
        )
    }

    /// Runs until [`Command::Quit`] (or until every command sender is gone, which quits
    /// too) and the instances the tray started have exited.
    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let Some(mut inbox) = self.inbox_receiver.take() else {
            return;
        };
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut commands_open = true;
        self.drive();
        self.publish();
        loop {
            tokio::select! {
                command = commands.recv(), if commands_open => match command {
                    Some(command) => self.command(command),
                    None => {
                        commands_open = false;
                        self.quit();
                    }
                },
                Some(message) = inbox.recv() => self.internal(message),
                _ = tick.tick() => {}
            }
            self.drive();
            self.publish();
            if self.quitting && self.instances.iter().all(|i| i.child.is_none()) {
                tracing::info!(event = "supervisor_quit");
                return;
            }
        }
    }

    /// Sets the configured instances (see [`Command::SetInstances`]).
    pub fn set_instances(&mut self, specs: Vec<InstanceSpec>) {
        let now = Instant::now();
        let mut seen = vec![false; self.instances.len()];
        for spec in specs {
            let id = instance_id(&spec.dir);
            if let Some(index) = self.index(&id) {
                if index < seen.len() {
                    seen[index] = true;
                }
                let instance = &mut self.instances[index];
                instance.autostart = spec.autostart;
                instance.task = spec.task;
                instance.removed = false;
                if matches!(instance.state, AgentState::External { .. }) {
                    let state = instance.external();
                    instance.set_state(state);
                }
                continue;
            }
            let autostart = spec.autostart && spec.task.is_none();
            self.instances.push(Instance::new(spec, id, now));
            if autostart && !self.quitting {
                let index = self.instances.len() - 1;
                self.begin_start(index, now);
            }
        }
        for (index, seen) in seen.into_iter().enumerate() {
            if seen {
                continue;
            }
            let instance = &mut self.instances[index];
            instance.removed = true;
            instance.restart_at = None;
            if instance.child.is_some() {
                if !instance.requested_stop {
                    self.stop(index, now);
                }
            } else {
                instance.client = None;
                instance.attempt = None;
                instance.next_connect = None;
            }
        }
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.instances.iter().position(|instance| instance.id == id)
    }

    fn index_of_dir(&self, dir: &Path) -> Option<usize> {
        let index = self.index(&instance_id(dir));
        if index.is_none() {
            tracing::warn!(event = "supervisor_unknown_instance");
        }
        index.filter(|index| !self.instances[*index].removed)
    }

    fn command(&mut self, command: Command) {
        if self.quitting {
            return;
        }
        let now = Instant::now();
        if let Some(dir) = command.action_dir() {
            if let Some(index) = self.index_of_dir(dir) {
                self.instances[index].refusal = None;
            }
        }
        match command {
            Command::SetInstances(specs) => self.set_instances(specs),
            Command::Start(dir) => {
                if let Some(index) = self.index_of_dir(&dir) {
                    self.start(index, now);
                }
            }
            Command::Stop(dir) => {
                if let Some(index) = self.index_of_dir(&dir) {
                    self.stop(index, now);
                }
            }
            Command::RestartBackend(dir) => {
                self.forward(&dir, AgentAction::RestartBackend, Call::RestartBackend);
            }
            Command::Reconnect(dir) => self.forward(&dir, AgentAction::Reconnect, Call::Reconnect),
            Command::Pause(dir) => self.forward(
                &dir,
                AgentAction::Pause,
                Call::PauseBackend(PauseParams {
                    reason: Some("paused from the tray".into()),
                    drain_timeout_ms: None,
                }),
            ),
            Command::Resume(dir) => self.forward(&dir, AgentAction::Resume, Call::ResumeBackend),
            Command::Refresh => {
                for instance in &mut self.instances {
                    if instance.client.is_some() {
                        instance.next_status = Some(now);
                    } else if instance.child.is_none()
                        && instance.attempt.is_none()
                        && instance.restart_at.is_none()
                        && !instance.removed
                        && matches!(instance.purpose, Purpose::Retry { .. })
                    {
                        instance.next_connect = Some(now);
                    }
                }
            }
            Command::Quit => self.quit(),
        }
    }

    fn start(&mut self, index: usize, now: Instant) {
        let instance = &mut self.instances[index];
        if instance.child.is_some() || instance.client.is_some() || instance.task_running {
            return;
        }
        instance.requested_stop = false;
        instance.restart_at = None;
        instance.last_error = None;
        instance.policy.reset();
        match instance.task.clone() {
            Some(task) => self.run_task(index, task),
            None => self.begin_start(index, now),
        }
    }

    /// Connects first (never both): reachable → external; nobody listening → spawn.
    fn begin_start(&mut self, index: usize, now: Instant) {
        let instance = &mut self.instances[index];
        instance.set_state(AgentState::Starting);
        instance.attempt = None;
        instance.purpose = Purpose::Start;
        instance.next_connect = Some(now);
    }

    fn run_task(&mut self, index: usize, task: TaskId) {
        let instance = &mut self.instances[index];
        instance.task_running = true;
        instance.attempt = None;
        instance.next_connect = None;
        instance.set_state(AgentState::Starting);
        tracing::info!(event = "bridge_task_run", path = %task.path, task = %task.name);
        let scheduler = Arc::clone(&self.scheduler);
        let inbox = self.inbox.clone();
        let id = instance.id.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || scheduler.run(&task))
                .await
                .unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
            let _ = inbox.send(Internal::TaskRun { id, result });
        });
    }

    fn spawn(&mut self, index: usize, now: Instant) {
        let instance = &mut self.instances[index];
        match self.spawner.spawn(&instance.dir) {
            Ok(child) => {
                tracing::info!(event = "bridge_spawned", pid = child.id());
                instance.child = Some(child);
                instance.spawned_at = now;
                instance.requested_stop = false;
                instance.set_state(AgentState::Starting);
                instance.retry(now, now + CONNECT_PATIENCE, CONNECT_RETRY);
            }
            Err(error) => {
                tracing::warn!(event = "bridge_spawn_failed", kind = ?error.kind());
                instance.last_error = Some(format!("could not start the bridge: {error}"));
                self.after_exit(index, None, Duration::ZERO, now);
            }
        }
    }

    fn stop(&mut self, index: usize, now: Instant) {
        let instance = &mut self.instances[index];
        instance.restart_at = None;
        if instance.child.is_some() {
            instance.requested_stop = true;
            instance.set_state(AgentState::Stopping);
            match instance.client.clone() {
                Some(client) => {
                    instance.kill_at = Some(now + SHUTDOWN_TIMEOUT + EXIT_GRACE);
                    self.shutdown(index, client);
                }
                // Not connected: connect to ask it; killed only if that fails.
                None => {
                    instance.kill_at =
                        Some(now + STOP_CONNECT_TIMEOUT + SHUTDOWN_TIMEOUT + EXIT_GRACE);
                    self.connect_and_shutdown(index);
                }
            }
        } else if let Some(client) = instance.client.clone() {
            instance.requested_stop = true;
            instance.set_state(AgentState::Stopping);
            self.shutdown(index, client);
        } else if !instance.task_running {
            instance.retry(now, now, PROBE_EVERY);
            if matches!(
                instance.state,
                AgentState::Starting | AgentState::Failed { .. }
            ) {
                instance.set_state(AgentState::Stopped);
            }
        }
    }

    fn shutdown(&self, index: usize, client: Arc<ControlClient>) {
        let inbox = self.inbox.clone();
        let id = self.instances[index].id.clone();
        tracing::info!(event = "bridge_shutdown_requested");
        tokio::spawn(async move {
            let result = client.call(Call::Shutdown, SHUTDOWN_TIMEOUT).await;
            let _ = inbox.send(Internal::ShutdownDone {
                id,
                result,
                fresh: false,
            });
        });
    }

    /// Opens a control channel of its own (for at most [`STOP_CONNECT_TIMEOUT`]) and sends
    /// `shutdown` on it, so the bridge declines its open cards before it goes.
    fn connect_and_shutdown(&self, index: usize) {
        let connector = Arc::clone(&self.connector);
        let inbox = self.inbox.clone();
        let instance = &self.instances[index];
        let id = instance.id.clone();
        let dir = instance.dir.clone();
        tracing::info!(event = "bridge_shutdown_requested", connect = true);
        tokio::spawn(async move {
            let result = match shutdown_over_new_connection(connector.as_ref(), &dir).await {
                Ok(Some(result)) => result,
                Ok(None) => Err(ErrorBody {
                    code: "not_connected".into(),
                    message: format!(
                        "the control channel did not answer within {} s",
                        STOP_CONNECT_TIMEOUT.as_secs()
                    ),
                }),
                Err(error) => Err(ErrorBody {
                    code: "not_connected".into(),
                    message: error.to_string(),
                }),
            };
            let _ = inbox.send(Internal::ShutdownDone {
                id,
                result,
                fresh: true,
            });
        });
    }

    fn forward(&mut self, dir: &Path, action: AgentAction, call: Call) {
        let Some(index) = self.index_of_dir(dir) else {
            return;
        };
        let method = call.method();
        let timeout = if matches!(call, Call::PauseBackend(_)) {
            PAUSE_TIMEOUT
        } else {
            CALL_TIMEOUT
        };
        let instance = &mut self.instances[index];
        let Some(client) = instance.client.clone() else {
            instance.last_error = Some(format!("{method}: not connected"));
            instance.refusal = Some(Refusal {
                action,
                message: "not connected".into(),
                at: now_ms(),
            });
            return;
        };
        let inbox = self.inbox.clone();
        let id = instance.id.clone();
        tokio::spawn(async move {
            let result = client.call(call, timeout).await;
            let _ = inbox.send(Internal::Called {
                id,
                method,
                action,
                result,
            });
        });
    }

    fn quit(&mut self) {
        let now = Instant::now();
        self.quitting = true;
        for index in 0..self.instances.len() {
            let instance = &mut self.instances[index];
            instance.restart_at = None;
            if instance.child.is_some() {
                if !instance.requested_stop {
                    self.stop(index, now);
                }
            } else if instance.client.is_none() && matches!(instance.purpose, Purpose::Start) {
                instance.retry(now, now, PROBE_EVERY);
                instance.set_state(AgentState::Stopped);
            }
        }
    }

    fn internal(&mut self, message: Internal) {
        let now = Instant::now();
        match message {
            Internal::Connected {
                id,
                attempt,
                result,
            } => {
                if let Some(index) = self.index(&id) {
                    self.connected(index, attempt, result, now);
                }
            }
            Internal::Event { id, conn, line } => {
                if let Some(index) = self.index(&id) {
                    self.event(index, conn, line);
                }
            }
            Internal::Disconnected { id, conn } => {
                if let Some(index) = self.index(&id) {
                    self.disconnected(index, conn, now);
                }
            }
            Internal::Status { id, conn, result } => {
                let Some(index) = self.index(&id) else {
                    return;
                };
                let instance = &mut self.instances[index];
                if instance.client.is_none() || instance.conn != conn {
                    return;
                }
                instance.status_pending = false;
                match *result {
                    Ok(status) => {
                        // Corrects a missed `state_changed` (a lagging connection skips
                        // events).
                        if let Some(state) = status.state {
                            let state = instance.shown(state, status.detail.clone());
                            if instance.reported.as_ref() != Some(&state) {
                                instance.report(state);
                            }
                        }
                        instance.status = Some(status);
                    }
                    Err(error) => {
                        tracing::debug!(event = "bridge_status_failed", code = %error.code);
                    }
                }
            }
            Internal::Called {
                id,
                method,
                action,
                result,
            } => {
                let Some(index) = self.index(&id) else {
                    return;
                };
                let instance = &mut self.instances[index];
                match result {
                    Ok(_) => {
                        instance.next_status = Some(now);
                        instance.refusal = None;
                    }
                    Err(error) => {
                        let message = format!("{} ({})", error.message, error.code);
                        instance.last_error = Some(format!("{method}: {message}"));
                        instance.refusal = Some(Refusal {
                            action,
                            message,
                            at: now_ms(),
                        });
                    }
                }
            }
            Internal::ShutdownDone { id, result, fresh } => {
                if let Some(index) = self.index(&id) {
                    self.shutdown_done(index, result, fresh, now);
                }
            }
            Internal::TaskRun { id, result } => {
                let Some(index) = self.index(&id) else {
                    return;
                };
                let instance = &mut self.instances[index];
                instance.task_running = false;
                match result {
                    Ok(()) => {
                        if instance.client.is_none() {
                            instance.retry(now, now + CONNECT_PATIENCE, CONNECT_RETRY);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(event = "bridge_task_run_failed", kind = ?error.kind());
                        instance.last_error = Some(format!("Scheduled Task: {error}"));
                        instance.set_state(AgentState::Failed {
                            code: None,
                            retry_in: None,
                        });
                        instance.retry(now, now, PROBE_EVERY);
                    }
                }
            }
        }
    }

    fn connected(
        &mut self,
        index: usize,
        attempt: u64,
        result: io::Result<Connection>,
        now: Instant,
    ) {
        let instance = &mut self.instances[index];
        if instance.attempt != Some(attempt) {
            return;
        }
        instance.attempt = None;
        match result {
            Ok((client, events)) => {
                if instance.removed {
                    return;
                }
                self.seq += 1;
                let conn = self.seq;
                instance.conn = conn;
                instance.client = Some(Arc::new(client));
                instance.status_pending = false;
                instance.next_status = Some(now);
                instance.next_connect = None;
                instance.purpose = Purpose::Retry { fast_until: now };
                if instance.child.is_none() {
                    instance.requested_stop = false;
                    let state = instance.external();
                    instance.set_state(state);
                }
                let inbox = self.inbox.clone();
                let id = instance.id.clone();
                tokio::spawn(forward_events(id, conn, events, inbox));
            }
            Err(error) => match instance.purpose {
                Purpose::Start => {
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) {
                        if self.quitting {
                            instance.retry(now, now, PROBE_EVERY);
                            instance.set_state(AgentState::Stopped);
                        } else {
                            self.spawn(index, now);
                        }
                    } else {
                        // Something listens but the channel failed: never start a second one.
                        instance.last_error = Some(format!("control channel: {error}"));
                        instance.next_connect = Some(now + Duration::from_secs(1));
                    }
                }
                Purpose::Retry { fast_until } => {
                    let fast = now < fast_until;
                    instance.next_connect =
                        Some(now + if fast { CONNECT_RETRY } else { PROBE_EVERY });
                    if !fast
                        && instance.child.is_none()
                        && !instance.task_running
                        && instance.state == AgentState::Starting
                    {
                        instance.set_state(AgentState::Stopped);
                    }
                }
            },
        }
    }

    fn event(&mut self, index: usize, conn: u64, line: EventLine) {
        let instance = &mut self.instances[index];
        if instance.client.is_none() || instance.conn != conn {
            return;
        }
        match line.event {
            Event::StateChanged { state, detail } => {
                let state = instance.shown(state, detail);
                instance.report(state);
            }
            Event::Error { code, method } => {
                instance.last_error = Some(match method {
                    Some(method) => format!("{method}: {code}"),
                    None => code,
                });
            }
            Event::PeekChanged => instance.peek_changes += 1,
            _ => {}
        }
    }

    fn disconnected(&mut self, index: usize, conn: u64, now: Instant) {
        let instance = &mut self.instances[index];
        if instance.client.is_none() || instance.conn != conn {
            return;
        }
        tracing::info!(event = "bridge_disconnected");
        instance.client = None;
        instance.reported = None;
        instance.status_pending = false;
        if instance.child.is_some() {
            if !instance.requested_stop {
                instance.set_state(AgentState::Starting);
                instance.retry(now, now + CONNECT_PATIENCE, CONNECT_RETRY);
            }
        } else {
            instance.requested_stop = false;
            instance.set_state(AgentState::Stopped);
            if !instance.removed {
                instance.retry(now, now + CONNECT_PATIENCE, CONNECT_RETRY);
            }
        }
    }

    fn shutdown_done(
        &mut self,
        index: usize,
        result: Result<Value, ErrorBody>,
        fresh: bool,
        now: Instant,
    ) {
        let instance = &mut self.instances[index];
        if let Err(error) = &result {
            instance.last_error = Some(format!("shutdown: {} ({})", error.message, error.code));
        }
        if instance.child.is_some() {
            if instance.kill_at.is_some() {
                // Could not even ask it: kill it now if it is still alive.
                let grace = if fresh && result.is_err() {
                    Duration::ZERO
                } else {
                    EXIT_GRACE
                };
                instance.kill_at = Some(now + grace);
            }
        } else if result.is_err() && instance.client.is_some() && instance.requested_stop {
            // The bridge did not take the request: show what it last reported.
            instance.requested_stop = false;
            let state = instance
                .reported
                .clone()
                .unwrap_or_else(|| instance.external());
            instance.set_state(state);
        }
    }

    /// Exits, kills, restarts, connection attempts and status refreshes that are due.
    fn drive(&mut self) {
        let now = Instant::now();
        let mut index = 0;
        while index < self.instances.len() {
            self.drive_one(index, now);
            let instance = &self.instances[index];
            if instance.removed && instance.child.is_none() && instance.client.is_none() {
                self.instances.remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn drive_one(&mut self, index: usize, now: Instant) {
        let exit = match self.instances[index]
            .child
            .as_mut()
            .map(|child| child.try_wait())
        {
            Some(Ok(exit)) => exit,
            Some(Err(error)) => {
                tracing::warn!(event = "bridge_wait_failed", kind = ?error.kind());
                None
            }
            None => None,
        };
        if let Some(exit) = exit {
            self.exited(index, exit.code, now);
        }
        let quitting = self.quitting;
        let instance = &mut self.instances[index];
        if instance.kill_at.is_some_and(|at| at <= now) {
            instance.kill_at = None;
            if let Some(child) = instance.child.as_mut() {
                tracing::warn!(event = "bridge_killed", pid = child.id());
                if let Err(error) = child.kill() {
                    tracing::warn!(event = "bridge_kill_failed", kind = ?error.kind());
                }
            }
        }
        if instance.restart_at.is_some_and(|at| at <= now) {
            instance.restart_at = None;
            if !quitting && instance.child.is_none() && instance.client.is_none() {
                self.begin_start(index, now);
            }
        }
        let instance = &self.instances[index];
        if instance.client.is_none()
            && instance.attempt.is_none()
            && instance.next_connect.is_some_and(|at| at <= now)
        {
            self.launch_connect(index);
        }
        let instance = &mut self.instances[index];
        if let Some(client) = instance.client.clone() {
            if !instance.status_pending && instance.next_status.is_some_and(|at| at <= now) {
                instance.status_pending = true;
                instance.next_status = Some(now + PROBE_EVERY);
                let inbox = self.inbox.clone();
                let id = instance.id.clone();
                let conn = instance.conn;
                tokio::spawn(async move {
                    let result = Box::new(client.status().await);
                    let _ = inbox.send(Internal::Status { id, conn, result });
                });
            }
        }
    }

    fn launch_connect(&mut self, index: usize) {
        self.seq += 1;
        let attempt = self.seq;
        let instance = &mut self.instances[index];
        instance.attempt = Some(attempt);
        instance.next_connect = None;
        let connector = Arc::clone(&self.connector);
        let inbox = self.inbox.clone();
        let id = instance.id.clone();
        let dir = instance.dir.clone();
        tokio::spawn(async move {
            let result = connector.connect(&dir).await;
            let _ = inbox.send(Internal::Connected {
                id,
                attempt,
                result,
            });
        });
    }

    fn exited(&mut self, index: usize, code: Option<i32>, now: Instant) {
        let instance = &mut self.instances[index];
        tracing::info!(event = "bridge_exited", code);
        let ran_for = now.saturating_duration_since(instance.spawned_at);
        instance.child = None;
        instance.kill_at = None;
        instance.client = None;
        instance.attempt = None;
        instance.status_pending = false;
        instance.last_exit = code;
        self.after_exit(index, code, ran_for, now);
    }

    fn after_exit(&mut self, index: usize, code: Option<i32>, ran_for: Duration, now: Instant) {
        let quitting = self.quitting;
        let instance = &mut self.instances[index];
        let requested = std::mem::take(&mut instance.requested_stop);
        let decision = instance
            .policy
            .on_exit(code, requested, ran_for, now.into_std());
        instance.retry(now, now, PROBE_EVERY);
        let notice = match decision {
            Decision::Restart(_) | Decision::Stopped if quitting => {
                instance.set_state(AgentState::Stopped);
                None
            }
            Decision::StoppedElsewhere => {
                instance.state = AgentState::Stopped;
                instance.history.push(now_ms(), STOPPED_ELSEWHERE);
                None
            }
            Decision::Restart(delay) => {
                instance.set_state(AgentState::Failed {
                    code,
                    retry_in: Some(delay),
                });
                instance.restart_at = Some(now + delay);
                None
            }
            Decision::Stopped => {
                instance.set_state(AgentState::Stopped);
                None
            }
            Decision::GiveUp => {
                instance.set_state(AgentState::GaveUp { code });
                Some(Notice::GaveUp {
                    dir: instance.dir.clone(),
                    name: instance.name(),
                    code,
                })
            }
            Decision::NotSetUp => {
                instance.set_state(AgentState::NotSetUp);
                Some(Notice::NotSetUp {
                    dir: instance.dir.clone(),
                    name: instance.name(),
                })
            }
        };
        if let Some(notice) = notice {
            if self.notices.try_send(notice).is_err() {
                tracing::warn!(event = "supervisor_notice_dropped");
            }
        }
    }

    fn publish(&self) {
        let views: Vec<InstanceView> = self.instances.iter().map(Instance::view).collect();
        self.views.send_if_modified(|current| {
            if *current == views {
                false
            } else {
                *current = views;
                true
            }
        });
    }
}

/// Nobody listens on the control channel.
fn nobody_listens(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Connects to the bridge in `dir` (for at most [`STOP_CONNECT_TIMEOUT`]) and sends
/// `shutdown`: `None` when the connection did not complete in time.
async fn shutdown_over_new_connection(
    connector: &dyn Connector,
    dir: &Path,
) -> io::Result<Option<Result<Value, ErrorBody>>> {
    match tokio::time::timeout(STOP_CONNECT_TIMEOUT, connector.connect(dir)).await {
        Ok(Ok((client, _events))) => Ok(Some(client.call(Call::Shutdown, SHUTDOWN_TIMEOUT).await)),
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(None),
    }
}

/// Makes sure the bridge in `dir` is gone before the tray starts its own: for the
/// switch-over, after the instance's Scheduled Task was stopped and removed (the bridge it
/// ran may survive that, under conhost). While the control channel is reachable it sends
/// `shutdown` (once it is taken) and looks again every [`CONNECT_RETRY`], for at most
/// `patience`. `Ok` once nobody listens; an error if a bridge (or anything that does not
/// answer as "nobody listens") is still there: then the caller must not start one.
pub async fn retire_task_bridge(
    connector: &dyn Connector,
    dir: &Path,
    patience: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + patience;
    let mut asked = false;
    loop {
        if asked {
            match tokio::time::timeout(STOP_CONNECT_TIMEOUT, connector.connect(dir)).await {
                Ok(Err(error)) if nobody_listens(&error) => return Ok(()),
                _ => {}
            }
        } else {
            match shutdown_over_new_connection(connector, dir).await {
                Err(error) if nobody_listens(&error) => return Ok(()),
                Ok(Some(Ok(_))) => {
                    tracing::info!(event = "switch_over", step = "shutdown_bridge");
                    asked = true;
                }
                _ => {}
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "the bridge in {} is still running {} s after its Scheduled Task was stopped{}; the tray does not start a second one",
                dir.display(),
                patience.as_secs(),
                if asked {
                    " and it was asked to shut down"
                } else {
                    ""
                },
            )));
        }
        tokio::time::sleep(CONNECT_RETRY).await;
    }
}

/// Hands a connection's events to the supervisor, then tells it the connection is gone.
async fn forward_events(
    id: String,
    conn: u64,
    mut events: mpsc::Receiver<EventLine>,
    inbox: mpsc::UnboundedSender<Internal>,
) {
    while let Some(line) = events.recv().await {
        if inbox
            .send(Internal::Event {
                id: id.clone(),
                conn,
                line,
            })
            .is_err()
        {
            return;
        }
    }
    let _ = inbox.send(Internal::Disconnected { id, conn });
}

#[cfg(test)]
mod tests;
