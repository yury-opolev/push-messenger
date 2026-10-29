//! The backend-independent core: the owner gate, turns as replies, cards, activity,
//! the owner's commands, and the backend's lifecycle. It drives a [`Backend`] and talks
//! to push-messenger through a [`Messenger`].

mod control;
mod conversation;
mod requests;
mod turns;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use push_sdk::{
    ActivityState, CardBook, ClientMsgId, ConnectionState, ConversationId, Event, SdkError, UserId,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use crate::backend::{
    Backend, BackendCommand, BackendError, BackendEvent, Capability, CommandReply, PermissionMode,
    Responder, TextFormat,
};
use crate::backoff::Backoff;
use crate::cards::CardKind;
use crate::messenger::Messenger;
use crate::render::{TurnRenderer, reply_bodies};
use crate::state::{State, StateFile};

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
}

impl<M: Messenger> Bridge<M> {
    pub fn new(
        messenger: Arc<M>,
        backend: Arc<dyn Backend>,
        backend_events: mpsc::UnboundedReceiver<BackendEvent>,
        messenger_events: mpsc::Receiver<Event>,
        state: Arc<StateFile>,
        options: BridgeOptions,
    ) -> Self {
        Self {
            messenger,
            backend,
            backend_events,
            messenger_events,
            state,
            options,
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
        } = self;
        let owner = messenger.owner().await.map_err(BridgeError::Messenger)?;
        let conversation = messenger
            .direct(owner)
            .await
            .map_err(BridgeError::Messenger)?;
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
        tracing::info!(event = "bridge_started", backend = core.name.as_str());
        core.boot().await;
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        let result = loop {
            let restart_at = core.restart_at;
            tokio::select! {
                biased;
                () = &mut shutdown => break Ok(()),
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
        };
        core.close().await;
        tracing::info!(event = "bridge_stopped", ok = result.is_ok());
        result
    }
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
    waiting_label: &'static str,
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

    backoff: Backoff,
    running_since: Option<Instant>,
    restart_at: Option<Instant>,
    /// Say so when the pending restart succeeds.
    announce_restart: bool,
    /// The last start failed (its error was shown once).
    start_failing: bool,
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
        Self {
            messenger,
            backend,
            name,
            format,
            state,
            folder: options.default_folder.clone(),
            mode: options.default_mode,
            options,
            internal,
            owner,
            conversation,
            clock: Clock::new(),
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
            backoff: Backoff::new(),
            running_since: None,
            restart_at: None,
            announce_restart: false,
            start_failing: false,
        }
    }

    fn has(&self, capability: Capability) -> bool {
        self.backend.capabilities().contains(&capability)
    }

    /// Sends `text`, the bridge's own words, as `agent.reply@1` replies (masked, paged).
    fn reply(&self, text: &str) {
        for body in reply_bodies(None, text) {
            self.send_rich(&body);
        }
    }

    fn send_rich(&self, body: &str) -> Option<ClientMsgId> {
        match self.messenger.send_rich(self.conversation, body) {
            Ok(id) => Some(id),
            Err(error) => {
                tracing::warn!(event = "send_failed", code = error.code());
                None
            }
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

    /// Waiting while a card is open, working while a turn runs, else idle.
    fn wanted_activity(&self) -> (ActivityState, Option<String>) {
        if let Some(request) = self.open.first() {
            (
                ActivityState::Waiting,
                Some(request.waiting_label.to_owned()),
            )
        } else if let Some(turn) = &self.turn {
            (ActivityState::Working, Some(turn.renderer.current_label()))
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
            }
        }
    }

    async fn on_tick(&mut self) {
        self.poll_turn();
        self.expire_requests().await;
        self.update_activity(false).await;
    }

    /// Ends what is open as the bridge stops: the reply, the cards (declined), the
    /// activity and the backend.
    async fn close(&mut self) {
        if let Some(mut turn) = self.turn.take() {
            turn.renderer.note("(The bridge stopped.)");
            let updates = turn.renderer.finish(false);
            self.apply(&mut turn.pages, updates);
        }
        for request in std::mem::take(&mut self.open) {
            if let Some(card) = request.card {
                self.cards.close(card);
            }
        }
        self.update_activity(false).await;
        self.backend.stop().await;
    }
}

/// The state file's key of `folder`.
fn folder_key(folder: &Path) -> String {
    folder.display().to_string()
}
