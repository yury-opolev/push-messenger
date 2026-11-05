//! The backend-independent core: the owner gate, turns as replies, cards, activity,
//! the owner's commands, and the backend's lifecycle. It drives a [`Backend`] and talks
//! to push-messenger through a [`Messenger`].

mod calling;
mod control;
mod conversation;
mod report;
mod requests;
mod turns;
mod upgrade;
mod voice;

#[cfg(test)]
mod echo_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod voice_tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use push_bridge_control::{
    AgentInfo, BridgeState, CallDirection, CallInfo, CallPhase, CardState, PeekKind,
};
use push_sdk::{
    ActivityState, CallEvent, CallId, CardBook, ClientMsgId, ConnectionState, ConversationId,
    Event, SdkError, UserId,
};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior};

use crate::backend::{
    ActivityLabels, Backend, BackendCommand, BackendError, BackendEvent, Capability, CommandReply,
    PermissionMode, Prepared, Responder, TextFormat,
};
use crate::backoff::Backoff;
use crate::cards::CardKind;
use crate::control::{Calls, ControlCall, ControlHandle};
use crate::messenger::Messenger;
use crate::render::{TurnRenderer, reply_bodies};
use crate::state::{State, StateFile};
use crate::voice::VoiceSetup;
use crate::voice::phone::Line;
use crate::voice::policy::Verdict;
use crate::voice::session::SessionEvent;

/// How often the core polls the reply, the cards' deadlines and the activity.
const TICK: Duration = Duration::from_millis(500);
/// Activity is sent again this often while it lasts (others show it for 30 s).
const ACTIVITY_REFRESH: Duration = Duration::from_secs(15);
/// A stranger hears "This agent is private." at most once per this.
const STRANGER_QUIET: Duration = Duration::from_secs(3_600);
/// The fixed reply to anyone but the owner.
pub const PRIVATE: &str = "This agent is private.";
/// Delivered messages remembered to handle a re-delivery once.
const SEEN: usize = 10_000;
/// A prompt that meets a busy agent with no turn to wait for is sent again after a pause:
/// the first, doubling up to the longest, while all the pauses add up to the budget.
const RETRY_FIRST: Duration = Duration::from_millis(500);
const RETRY_LONGEST: Duration = Duration::from_secs(8);
const RETRY_BUDGET: Duration = Duration::from_secs(30);

/// The pause before the `retry`th retry (from 1), or `None` once the budget is spent.
fn retry_delay(retry: u32) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut delay = RETRY_FIRST;
    for _ in 1..retry {
        total += delay;
        delay = (delay * 2).min(RETRY_LONGEST);
    }
    (retry > 0 && total + delay <= RETRY_BUDGET).then_some(delay)
}

/// Where the agent may work and how it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeOptions {
    /// `/cd` accepts only folders inside these.
    pub roots: Vec<PathBuf>,
    pub default_folder: PathBuf,
    /// The permission mode of a new session (and after `/new`).
    pub default_mode: PermissionMode,
}

/// Why the bridge stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BridgeError {
    /// The agent's owner or conversation could not be found at the start.
    #[error("messenger: {0}")]
    Messenger(SdkError),
    /// The messenger's credentials are no longer accepted: sign in again.
    #[error("signed out")]
    SignedOut,
    /// The server removed this device.
    #[error("this device was removed")]
    DeviceRemoved,
    /// Another copy of the bridge's data took over the connection.
    #[error("the bridge's data was opened elsewhere")]
    OpenedElsewhere,
    /// The messenger's events ended.
    #[error("the messenger's events ended")]
    Closed,
}

/// The bridge: one agent backend reached by its owner through push-messenger.
pub struct Bridge<M: Messenger> {
    messenger: Arc<M>,
    backend: Arc<dyn Backend>,
    backend_events: mpsc::UnboundedReceiver<BackendEvent>,
    messenger_events: mpsc::Receiver<Event>,
    state: Arc<StateFile>,
    options: BridgeOptions,
    control: ControlHandle,
    calls: Calls,
    /// Keeps a bridge without a control channel's actions channel open.
    _calls: Option<mpsc::Sender<ControlCall>>,
    /// Takes the owner's calls; `None` ignores call events.
    voice: Option<VoiceSetup>,
    /// Set while a call is in progress (see [`Bridge::with_call_flag`]).
    call_flag: Option<CallFlag>,
}

/// `true` while a call rings, is asked about, being answered or placed, or runs, and
/// once the bridge is stopping: calls advertising waits for it ([`crate::voice::adapter::advertise`]).
pub type CallFlag = Arc<watch::Sender<bool>>;

impl<M: Messenger> Bridge<M> {
    pub fn new(
        messenger: Arc<M>,
        backend: Arc<dyn Backend>,
        backend_events: mpsc::UnboundedReceiver<BackendEvent>,
        messenger_events: mpsc::Receiver<Event>,
        state: Arc<StateFile>,
        options: BridgeOptions,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(1);
        Self {
            messenger,
            backend,
            backend_events,
            messenger_events,
            state,
            options,
            control: ControlHandle::detached(),
            calls: Arc::new(tokio::sync::Mutex::new(receiver)),
            _calls: Some(sender),
            voice: None,
            call_flag: None,
        }
    }

    /// Takes the owner's calls with `voice` (see [`crate::voice`]).
    pub fn with_voice(self, voice: VoiceSetup) -> Self {
        Self {
            voice: Some(voice),
            ..self
        }
    }

    /// Keeps `flag` `true` while a call is in progress and once the bridge is stopping
    /// (advertising holds still through the shutdown).
    pub fn with_call_flag(self, flag: CallFlag) -> Self {
        Self {
            call_flag: Some(flag),
            ..self
        }
    }

    /// Reports to the control channel through `handle` and takes its actions from
    /// `calls` while it runs.
    pub fn with_control(self, handle: ControlHandle, calls: Calls) -> Self {
        Self {
            control: handle,
            calls,
            _calls: None,
            ..self
        }
    }

    /// Runs until `shutdown` resolves (`Ok`) or the messenger stops it (signed out, device
    /// removed, events ended); returns why it stopped. The backend is stopped either way.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send) -> Result<(), BridgeError> {
        let Self {
            messenger,
            backend,
            mut backend_events,
            mut messenger_events,
            state,
            options,
            control,
            calls,
            _calls,
            voice,
            call_flag,
        } = self;
        let mut calls = calls.lock().await;
        let owner = messenger.owner().await.map_err(BridgeError::Messenger)?;
        let conversation = messenger
            .direct(owner)
            .await
            .map_err(BridgeError::Messenger)?;
        if let Some((display_name, handle)) = messenger.profile().await {
            control.update(|status| {
                status.agent = Some(AgentInfo {
                    display_name,
                    handle,
                });
            });
        }
        let (internal, mut internal_events) = mpsc::unbounded_channel();
        let mut core = Core::new(
            messenger,
            backend,
            state,
            options,
            internal,
            owner,
            conversation,
        );
        core.control = control;
        core.voice = voice;
        core.call_flag = call_flag;
        tracing::info!(event = "bridge_started", backend = core.name.as_str());
        core.boot().await;
        core.publish();
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        let result = loop {
            let restart_at = core.restart_at;
            tokio::select! {
                biased;
                () = &mut shutdown => break Ok(()),
                Some(call) = calls.recv() => match core.on_control(call).await {
                    Flow::Continue => {}
                    Flow::Stop => break Ok(()),
                },
                Some(event) = backend_events.recv() => core.on_backend(event).await,
                Some(done) = internal_events.recv() => core.on_internal(done).await,
                event = messenger_events.recv() => match event {
                    Some(event) => {
                        if let Err(error) = core.on_messenger(event).await {
                            break Err(error);
                        }
                    }
                    None => break Err(BridgeError::Closed),
                },
                () = tokio::time::sleep_until(restart_at.unwrap_or_else(Instant::now)),
                    if restart_at.is_some() => core.restart().await,
                _ = ticker.tick() => core.on_tick().await,
            }
            core.publish();
        };
        core.close().await;
        tracing::info!(event = "bridge_stopped", ok = result.is_ok());
        result
    }
}

/// Whether the run goes on after a control action.
enum Flow {
    Continue,
    Stop,
}

/// Results of backend calls the core does not wait for.
enum Internal {
    Prompted {
        text: String,
        attempt: u32,
        result: Result<(), BackendError>,
    },
    /// The pause before prompting `text` again is over.
    Retry { text: String, attempt: u32 },
    Commanded {
        command: BackendCommand,
        /// The folder and session the command ran in.
        folder: PathBuf,
        session: Option<String>,
        result: Result<CommandReply, BackendError>,
    },
    /// `prepare_restart` answered for the pause with this id.
    Prepared {
        pause: u64,
        result: Result<Prepared, BackendError>,
    },
    /// The phone answered (or failed to answer) the owner's call.
    Answered {
        call_id: CallId,
        result: Result<Line, SdkError>,
    },
    /// The pipeline of call `call_id` reported `event`.
    Voice {
        call_id: CallId,
        event: SessionEvent,
    },
    /// The phone placed (or failed to place) the call to the owner.
    Dialed { result: Result<Line, SdkError> },
}

/// The running turn's reply.
struct Turn {
    renderer: TurnRenderer,
    /// The message of each page, by page index.
    pages: Vec<Option<ClientMsgId>>,
}

/// A request waiting for the owner: a backend's (with its responder) or `/mode bypass`'s.
struct OpenRequest {
    kind: CardKind,
    /// `None` for `/mode bypass`, which the core answers itself.
    responder: Option<Responder>,
    /// The card; `None` for a question shown as text only.
    card: Option<ClientMsgId>,
    deadline: Instant,
    /// When the card was posted: only an utterance begun after it answers it.
    opened: Instant,
    waiting_label: String,
    /// The card's title as shown (a voice answer keeps it).
    title: String,
    /// What a call says about the request.
    spoken: String,
    /// The bridge called the owner about it (`notify_by_call`): at most once.
    notified: bool,
}

/// Unix ms that follow tokio's clock (paused in tests).
struct Clock {
    wall_ms: i64,
    base: Instant,
}

impl Clock {
    fn new() -> Self {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            wall_ms: i64::try_from(wall.as_millis()).unwrap_or(i64::MAX),
            base: Instant::now(),
        }
    }

    fn now_ms(&self) -> i64 {
        let elapsed = i64::try_from(self.base.elapsed().as_millis()).unwrap_or(i64::MAX);
        self.wall_ms.saturating_add(elapsed)
    }
}

struct Core<M: Messenger> {
    messenger: Arc<M>,
    backend: Arc<dyn Backend>,
    name: String,
    /// How the backend writes its turns and background messages.
    format: TextFormat,
    /// The backend's activity labels.
    labels: ActivityLabels,
    /// How long a card waits.
    answer_timeout: Duration,
    /// The owner's texts stored before this (server time, µs) are not delivered.
    skip_before_us: Option<i64>,
    state: Arc<StateFile>,
    options: BridgeOptions,
    internal: mpsc::UnboundedSender<Internal>,
    owner: UserId,
    conversation: ConversationId,
    clock: Clock,

    folder: PathBuf,
    session: Option<String>,
    mode: PermissionMode,
    version: Option<String>,
    engine_instance: Option<String>,

    turn: Option<Turn>,
    /// The owner's texts waiting for the turn (or a slot-holding command) to end.
    queued: Vec<String>,
    /// Backend commands running that keep the agent busy without a turn (`/compact`,
    /// `/fork`): texts wait for them.
    slot_commands: usize,
    /// `/new` came during a turn: start it when the turn ends.
    new_after_turn: bool,

    open: Vec<OpenRequest>,
    cards: CardBook,

    activity: (ActivityState, Option<String>),
    activity_sent: Option<Instant>,

    strangers: HashMap<UserId, Instant>,
    seen: HashSet<(ConversationId, i64)>,
    seen_order: VecDeque<(ConversationId, i64)>,
    connection: Option<ConnectionState>,

    /// The control channel: the status snapshot and the events.
    control: ControlHandle,
    /// The state last reported, with its detail.
    reported: Option<(BridgeState, Option<String>)>,
    /// The bridge is closing.
    stopping: bool,
    /// The backend's tasks that started and have not completed.
    running_tasks: HashSet<String>,

    backoff: Backoff,
    running_since: Option<Instant>,
    restart_at: Option<Instant>,
    /// Say so when the pending restart succeeds.
    announce_restart: bool,
    /// The last start failed (its error was shown once).
    start_failing: bool,

    /// Upgrade mode: the backend is paused (see [`upgrade`]).
    pause: Option<upgrade::Pause>,
    /// Pauses so far (each one's id).
    pauses: u64,
    /// The backend saved its state for a restart that did not happen: the next start
    /// of the current session restores it.
    restore_next: bool,
    /// A start after a pause failed: the next successful start says how the backend
    /// came back and asks to confirm approvals again.
    pending_resume: Option<upgrade::ResumeCarry>,

    /// Takes the owner's calls (see [`voice`]); `None` ignores call events.
    voice: Option<VoiceSetup>,
    /// The call running (or ringing the owner).
    call: Option<voice::ActiveCall>,
    /// The call being answered, with its call events that came meanwhile.
    answering: Option<(CallId, Vec<CallEvent>)>,
    /// The owner's call waiting for the owner's answer on a card (`calls.answer` `ask`).
    asking: Option<calling::Asking>,
    /// A call to the owner being placed.
    dialing: Option<calling::Dialing>,
    /// When the bridge last called the owner (the rate limit).
    last_dialed: Option<Instant>,
    /// Why the last due call was held back (logged once per change).
    held_by: Option<Verdict>,
    /// Whether calls are held back because nothing can speak (logged once per change).
    held_no_tts: bool,
    /// The call's phase and direction, and when that phase began (Unix ms).
    call_since: Option<((CallPhase, CallDirection), i64)>,
    /// The call last reported to the control channel.
    call_reported: Option<CallInfo>,
    /// The tray muted the call being answered: its session starts muted.
    mute_next: bool,
    /// Set while a call is in progress.
    call_flag: Option<CallFlag>,
    /// The voice adapter last reported to the control channel.
    adapter_reports: crate::voice::adapter::report::AdapterReports,
}

impl<M: Messenger> Core<M> {
    fn new(
        messenger: Arc<M>,
        backend: Arc<dyn Backend>,
        state: Arc<StateFile>,
        options: BridgeOptions,
        internal: mpsc::UnboundedSender<Internal>,
        owner: UserId,
        conversation: ConversationId,
    ) -> Self {
        let name = backend.name().to_owned();
        let format = backend.text_format();
        let labels = backend.activity_labels();
        let answer_timeout = backend.answer_timeout();
        let clock = Clock::new();
        let skip_before_us = backend.skip_texts_older_than().map(|age| {
            let age_ms = i64::try_from(age.as_millis()).unwrap_or(i64::MAX);
            clock.now_ms().saturating_sub(age_ms).saturating_mul(1000)
        });
        Self {
            messenger,
            backend,
            name,
            format,
            labels,
            answer_timeout,
            skip_before_us,
            state,
            folder: options.default_folder.clone(),
            mode: options.default_mode,
            options,
            internal,
            owner,
            conversation,
            clock,
            session: None,
            version: None,
            engine_instance: None,
            turn: None,
            queued: Vec::new(),
            slot_commands: 0,
            new_after_turn: false,
            open: Vec::new(),
            cards: CardBook::new(),
            activity: (ActivityState::Idle, None),
            activity_sent: None,
            strangers: HashMap::new(),
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            connection: None,
            control: ControlHandle::detached(),
            reported: None,
            stopping: false,
            running_tasks: HashSet::new(),
            backoff: Backoff::new(),
            running_since: None,
            restart_at: None,
            announce_restart: false,
            start_failing: false,
            pause: None,
            pauses: 0,
            restore_next: false,
            pending_resume: None,
            voice: None,
            call: None,
            answering: None,
            asking: None,
            dialing: None,
            last_dialed: None,
            held_by: None,
            held_no_tts: false,
            call_since: None,
            call_reported: None,
            mute_next: false,
            call_flag: None,
            adapter_reports: Default::default(),
        }
    }

    fn has(&self, capability: Capability) -> bool {
        self.backend.capabilities().contains(&capability)
    }

    /// Sends `text`, the bridge's own words, as `agent.reply@1` replies (masked, paged).
    fn reply(&self, text: &str) {
        for body in reply_bodies(None, text) {
            self.send_rich_as(&body, PeekKind::Note);
        }
    }

    /// Sends `body` for the agent (its text is a reply in the peek).
    fn send_rich(&self, body: &str) -> Option<ClientMsgId> {
        self.send_rich_as(body, PeekKind::Reply)
    }

    /// Sends `body` and records it in the peek, its text as `kind`.
    fn send_rich_as(&self, body: &str, kind: PeekKind) -> Option<ClientMsgId> {
        match self.messenger.send_rich(self.conversation, body) {
            Ok(id) => {
                let time = self.clock.now_ms();
                self.control
                    .record(|ring| ring.sent(time, Some(id), body, kind));
                Some(id)
            }
            Err(error) => {
                tracing::warn!(event = "send_failed", code = error.code());
                None
            }
        }
    }

    /// Sets the peek's state of `card` (if any).
    fn peek_card(&self, card: Option<ClientMsgId>, state: CardState) {
        if let Some(card) = card {
            self.control.record(|ring| {
                ring.card_state(card, state);
            });
        }
    }

    /// Changes the state file; a failed save is logged.
    fn save(&self, change: impl FnOnce(&mut State)) {
        if self.state.update(change).is_err() {
            tracing::warn!(event = "state_save_failed");
        }
    }

    /// Sends the activity when it changed, or again when it is due for a refresh
    /// (or `force`, after a reconnect). Idle is sent once.
    async fn update_activity(&mut self, force: bool) {
        let wanted = self.wanted_activity();
        let now = Instant::now();
        let changed = wanted != self.activity;
        let lasting = wanted.0 != ActivityState::Idle;
        let due = lasting
            && self
                .activity_sent
                .is_none_or(|sent| now.saturating_duration_since(sent) >= ACTIVITY_REFRESH);
        if !(changed || due || (force && lasting)) {
            return;
        }
        self.activity = wanted.clone();
        self.activity_sent = Some(now);
        let (state, label) = wanted;
        if let Err(error) = self
            .messenger
            .activity(self.conversation, state, label.as_deref())
            .await
        {
            tracing::debug!(event = "activity_failed", code = error.code());
        }
    }

    /// Waiting while a card is open, working while paused, a call runs or a turn runs,
    /// else idle.
    fn wanted_activity(&self) -> (ActivityState, Option<String>) {
        if let Some(request) = self.open.first() {
            (
                ActivityState::Waiting,
                Some(request.waiting_label.to_owned()),
            )
        } else if self.pause.is_some() {
            (ActivityState::Working, Some(self.pause_label()))
        } else if let Some(label) = self.call_label() {
            (ActivityState::Working, Some(label.to_owned()))
        } else if let Some(turn) = &self.turn {
            let label = turn
                .renderer
                .running_label()
                .unwrap_or_else(|| self.labels.working.clone());
            (ActivityState::Working, Some(label))
        } else {
            (ActivityState::Idle, None)
        }
    }

    async fn on_internal(&mut self, done: Internal) {
        match done {
            Internal::Prompted {
                text,
                attempt,
                result,
            } => self.prompted(text, attempt, result).await,
            Internal::Retry { text, attempt } => self.deliver(text, attempt).await,
            Internal::Commanded {
                command,
                folder,
                session,
                result,
            } => {
                self.commanded(command, &folder, session.as_deref(), result);
                self.send_queued();
                self.release_when_idle().await;
            }
            Internal::Prepared { pause, result } => self.prepared(pause, result).await,
            Internal::Answered { call_id, result } => self.answered(call_id, result).await,
            Internal::Voice { call_id, event } => self.on_voice(call_id, event).await,
            Internal::Dialed { result } => self.dialed(result).await,
        }
    }

    async fn on_tick(&mut self) {
        self.poll_turn();
        self.expire_requests().await;
        self.expire_asking();
        self.call_owner_if_due().await;
        self.watch_pause().await;
        self.update_activity(false).await;
    }

    /// Ends what is open as the bridge stops: the reply, the requests (declined, said
    /// once), the activity and the backend.
    async fn close(&mut self) {
        self.stopping = true;
        self.sync_call_flag();
        if let Some(call) = self.end_call_at_close() {
            call.session.shut_down().await;
        }
        self.end_pause_at_close();
        self.publish();
        if let Some(mut turn) = self.turn.take() {
            turn.renderer.note("(The bridge stopped.)");
            let updates = turn.renderer.finish(false);
            self.apply(&mut turn.pages, updates);
            self.report_turn_ended(true);
        }
        let open = std::mem::take(&mut self.open);
        for request in &open {
            if let Some(card) = request.card {
                self.cards.close(card);
            }
            self.peek_card(request.card, CardState::Declined);
            self.request_closed(&request.kind, "declined");
        }
        if !open.is_empty() {
            drop(open);
            self.reply("Declined: the bridge was stopped.");
        }
        self.update_activity(false).await;
        self.publish();
        self.backend.stop().await;
        self.running_since = None;
        self.running_tasks.clear();
        self.publish();
    }
}

/// The state file's key of `folder`.
fn folder_key(folder: &Path) -> String {
    folder.display().to_string()
}
