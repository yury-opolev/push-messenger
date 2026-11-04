//! The phone: answering and declining the owner's calls, calling the owner, and a call's
//! [`Line`]: the owner's audio in, the agent's audio out, the call's signalling and
//! lifetime. The app uses push-sdk's media helper (`media::MediaPhone`, feature `voice`);
//! tests use [`FakePhone`], whose [`FakeCall`] is the owner's side of a call.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use push_sdk::{CallEndReason, CallEvent, CallId, SdkError, UserId};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use super::{Frame, frames};
use crate::messenger::wait;

/// How a call ended, as the server or the media said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineEnded {
    pub reason: CallEndReason,
    pub duration_s: Option<u32>,
}

/// Where the agent's audio goes.
#[async_trait::async_trait]
pub trait Speaker: Send + Sync + 'static {
    /// Queues a frame (waits while the queue is full); `false` once the call ended.
    async fn send(&self, frame: Frame) -> bool;
    /// Drops what is queued: stop speaking at once.
    fn clear(&self);
}

/// The call's signalling and lifetime.
#[async_trait::async_trait]
pub trait LineControl: Send + Sync + 'static {
    /// Gives the line a call event; `true` when it was about this call.
    fn handle(&self, event: &CallEvent) -> bool;
    /// Waits until audio flows; `Err` with the end if it ends first.
    async fn connected(&self) -> Result<(), LineEnded>;
    /// Waits for the end of the call.
    async fn ended(&self) -> LineEnded;
    /// Hangs up (completed); returns once the call ended.
    async fn hangup(&self) -> LineEnded;
}

/// An answered call: the owner's audio in, the agent's audio out.
pub struct Line {
    pub call_id: CallId,
    /// The owner's audio; closes when the call ends.
    pub incoming: mpsc::Receiver<Frame>,
    pub speaker: Arc<dyn Speaker>,
    pub control: Arc<dyn LineControl>,
}

/// Answers, declines and places calls (the media helper in the app, a fake in tests).
#[async_trait::async_trait]
pub trait Phone: Send + Sync + 'static {
    async fn answer(&self, call_id: CallId, offer_sdp: &str) -> Result<Line, SdkError>;
    async fn decline(&self, call_id: CallId, busy: bool) -> Result<(), SdkError>;
    /// Calls `peer`: returns once the call rings. The line's `connected` waits for the
    /// answer; a call that is not answered ends (missed, declined, unreachable, …)
    /// before it connects.
    async fn call(&self, peer: UserId) -> Result<Line, SdkError>;
}

/// Whether `event` is about `call_id` (as push-sdk's media helper decides).
pub fn is_about(event: &CallEvent, call_id: CallId) -> bool {
    match event {
        CallEvent::Incoming { call_id: id, .. }
        | CallEvent::Accepted { call_id: id, .. }
        | CallEvent::Taken { call_id: id }
        | CallEvent::Ended { call_id: id, .. }
        | CallEvent::PeerIce { call_id: id, .. }
        | CallEvent::PeerRenegotiate { call_id: id, .. }
        | CallEvent::Error { call_id: id, .. } => *id == call_id,
        CallEvent::Synced { calls, gone } => {
            gone.contains(&call_id) || calls.iter().any(|call| call.call_id == call_id)
        }
        CallEvent::HistoryChanged { .. } => false,
    }
}

/// Frames the fake's speaker queues ahead (5 s, as the media helper).
const FAKE_QUEUE: usize = 250;
/// Frames of the owner's audio in flight to the bridge.
const FAKE_INCOMING: usize = 250;
const FRAME_TIME: Duration = Duration::from_millis(20);

/// A phone for tests: answers every call (unless told otherwise) with a [`FakeCall`].
pub struct FakePhone {
    state: Mutex<PhoneState>,
    changed: Notify,
}

#[derive(Default)]
struct PhoneState {
    fail_next: Option<SdkError>,
    hold_next: Option<oneshot::Receiver<()>>,
    connect_delay: Option<Duration>,
    declined: Vec<(CallId, bool)>,
    calls: Vec<FakeCall>,
    /// Calls [`FakePhone::wait_answered`] returned so far.
    returned: usize,
    fail_next_call: Option<SdkError>,
    /// The calls placed, with whom they ring.
    dialed: Vec<(UserId, FakeCall)>,
    /// Calls [`FakePhone::wait_dialed`] returned so far.
    dialed_returned: usize,
}

impl FakePhone {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PhoneState::default()),
            changed: Notify::new(),
        })
    }

    fn state(&self) -> MutexGuard<'_, PhoneState> {
        self.state.lock().expect("the fake phone is not poisoned")
    }

    /// The next answer fails with `error`.
    pub fn fail_next_answer(&self, error: SdkError) {
        self.state().fail_next = Some(error);
    }

    /// The next answer waits until the returned sender is used (or dropped).
    pub fn hold_next_answer(&self) -> oneshot::Sender<()> {
        let (sender, receiver) = oneshot::channel();
        self.state().hold_next = Some(receiver);
        sender
    }

    /// Calls answered from now on connect after `delay` (at once by default).
    pub fn delay_connect(&self, delay: Duration) {
        self.state().connect_delay = Some(delay);
    }

    /// The calls declined: (call, busy).
    pub fn declined(&self) -> Vec<(CallId, bool)> {
        self.state().declined.clone()
    }

    /// The calls answered, in order.
    pub fn answered(&self) -> Vec<CallId> {
        self.state().calls.iter().map(|call| call.call_id).collect()
    }

    /// The owner's side of the latest answered call that this did not return yet; panics
    /// after 60 s.
    pub async fn wait_answered(&self) -> FakeCall {
        wait(&self.changed, "an answered call", || {
            let state = self.state();
            state.calls.len() > state.returned
        })
        .await;
        let mut state = self.state();
        state.returned = state.calls.len();
        state.calls.last().cloned().expect("a call was answered")
    }

    /// Waits until `count` calls were declined; panics after 60 s.
    pub async fn wait_declined(&self, count: usize) {
        wait(&self.changed, "declined calls", || {
            self.state().declined.len() >= count
        })
        .await;
    }

    /// The next call placed fails with `error`.
    pub fn fail_next_call(&self, error: SdkError) {
        self.state().fail_next_call = Some(error);
    }

    /// Whom the calls placed so far rang, in order.
    pub fn dialed(&self) -> Vec<UserId> {
        self.state().dialed.iter().map(|(peer, _)| *peer).collect()
    }

    /// The owner's side of the latest placed call that this did not return yet (it
    /// rings until [`FakeCall::pick_up`] or [`FakeCall::end`]); panics after 60 s.
    pub async fn wait_dialed(&self) -> FakeCall {
        wait(&self.changed, "a placed call", || {
            let state = self.state();
            state.dialed.len() > state.dialed_returned
        })
        .await;
        let mut state = self.state();
        state.dialed_returned = state.dialed.len();
        state
            .dialed
            .last()
            .map(|(_, call)| call.clone())
            .expect("a call was placed")
    }
}

#[async_trait::async_trait]
impl Phone for FakePhone {
    async fn answer(&self, call_id: CallId, _offer_sdp: &str) -> Result<Line, SdkError> {
        let (fail, hold, delay) = {
            let mut state = self.state();
            (
                state.fail_next.take(),
                state.hold_next.take(),
                state.connect_delay,
            )
        };
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        if let Some(error) = fail {
            return Err(error);
        }
        let (call, line) = FakeCall::open(call_id, delay, false);
        self.state().calls.push(call);
        self.changed.notify_waiters();
        Ok(line)
    }

    async fn decline(&self, call_id: CallId, busy: bool) -> Result<(), SdkError> {
        self.state().declined.push((call_id, busy));
        self.changed.notify_waiters();
        Ok(())
    }

    async fn call(&self, peer: UserId) -> Result<Line, SdkError> {
        let (fail, delay) = {
            let mut state = self.state();
            (state.fail_next_call.take(), state.connect_delay)
        };
        if let Some(error) = fail {
            return Err(error);
        }
        let (call, line) = FakeCall::open(CallId::new(), delay, true);
        self.state().dialed.push((peer, call));
        self.changed.notify_waiters();
        Ok(line)
    }
}

/// The owner's side of a call [`FakePhone`] answered. Its speaker plays one queued frame
/// per 20 ms (queueing up to 5 s, as the media helper); [`FakeCall::heard`] is what was
/// played.
#[derive(Clone)]
pub struct FakeCall {
    pub call_id: CallId,
    shared: Arc<CallShared>,
}

struct CallShared {
    inner: Mutex<CallInner>,
    changed: Notify,
    ended: watch::Sender<Option<LineEnded>>,
    /// Bumped by `clear()`: frames queued before are dropped.
    generation: AtomicU64,
    started: Instant,
    connect_delay: Option<Duration>,
    /// The owner answered (at once for a call the bridge answered).
    picked_up: watch::Sender<bool>,
    /// Runs on the speaker's next `clear()`.
    on_clear: Mutex<Option<ClearHook>>,
}

type ClearHook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct CallInner {
    /// The owner's audio to the bridge; `None` once the call ended.
    sender: Option<mpsc::Sender<Frame>>,
    heard: Vec<i16>,
    heard_since_clear: Vec<i16>,
    /// When each heard frame started to play.
    played_at: Vec<tokio::time::Instant>,
    clears: usize,
    hung_up: bool,
    handled: Vec<CallEvent>,
}

impl CallShared {
    fn inner(&self) -> MutexGuard<'_, CallInner> {
        self.inner.lock().expect("the fake call is not poisoned")
    }

    fn ended_now(&self) -> Option<LineEnded> {
        *self.ended.borrow()
    }

    /// Ends the call once: `incoming` closes, the speaker stops.
    fn finish(&self, reason: CallEndReason) {
        let duration_s = u32::try_from(self.started.elapsed().as_secs()).unwrap_or(u32::MAX);
        self.finish_with(LineEnded {
            reason,
            duration_s: Some(duration_s),
        });
    }

    fn finish_with(&self, end: LineEnded) {
        self.ended.send_if_modified(|ended| {
            if ended.is_some() {
                return false;
            }
            *ended = Some(end);
            true
        });
        self.inner().sender = None;
        self.changed.notify_waiters();
    }

    async fn wait_ended(&self) -> LineEnded {
        let mut ended = self.ended.subscribe();
        match ended.wait_for(Option::is_some).await {
            Ok(end) => end.expect("waited for the end"),
            // The sender lives as long as `self`.
            Err(_) => LineEnded {
                reason: CallEndReason::Failed,
                duration_s: None,
            },
        }
    }
}

impl FakeCall {
    /// A call; `ringing` until the owner picks up (a call the bridge placed).
    fn open(call_id: CallId, connect_delay: Option<Duration>, ringing: bool) -> (Self, Line) {
        let (sender, incoming) = mpsc::channel(FAKE_INCOMING);
        let (ended, _) = watch::channel(None);
        let shared = Arc::new(CallShared {
            inner: Mutex::new(CallInner {
                sender: Some(sender),
                ..CallInner::default()
            }),
            changed: Notify::new(),
            ended,
            generation: AtomicU64::new(0),
            started: Instant::now(),
            connect_delay,
            picked_up: watch::Sender::new(!ringing),
            on_clear: Mutex::new(None),
        });
        let (queue, played) = mpsc::channel(FAKE_QUEUE);
        tokio::spawn(play(shared.clone(), played));
        let line = Line {
            call_id,
            incoming,
            speaker: Arc::new(FakeSpeaker {
                queue,
                shared: shared.clone(),
            }),
            control: Arc::new(FakeControl {
                call_id,
                shared: shared.clone(),
            }),
        };
        (Self { call_id, shared }, line)
    }

    /// The owner says `samples`: 20 ms frames into the bridge as fast as it takes them.
    pub async fn say(&self, samples: &[i16]) {
        for frame in frames(samples) {
            if !self.send(frame).await {
                return;
            }
        }
    }

    /// The owner says `samples` in real time: one frame per 20 ms.
    pub async fn say_paced(&self, samples: &[i16]) {
        let mut ticks = tokio::time::interval(FRAME_TIME);
        for frame in frames(samples) {
            ticks.tick().await;
            if !self.send(frame).await {
                return;
            }
        }
    }

    async fn send(&self, frame: Frame) -> bool {
        let sender = self.shared.inner().sender.clone();
        match sender {
            Some(sender) => sender.send(frame).await.is_ok(),
            None => false,
        }
    }

    /// Everything the bridge's speaker played, in order.
    pub fn heard(&self) -> Vec<i16> {
        self.shared.inner().heard.clone()
    }

    /// What the speaker played since its last `clear()`.
    pub fn heard_since_clear(&self) -> Vec<i16> {
        self.shared.inner().heard_since_clear.clone()
    }

    /// When each frame [`FakeCall::heard`] holds started to play, in order.
    pub fn played_at(&self) -> Vec<tokio::time::Instant> {
        self.shared.inner().played_at.clone()
    }

    /// How often the bridge cleared its speaker.
    pub fn clears(&self) -> usize {
        self.shared.inner().clears
    }

    /// Runs `hook` in the speaker's next `clear()`, in the bridge's task that clears it
    /// (a barge-in's or a hush's), before that task goes on.
    pub fn on_next_clear(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .shared
            .on_clear
            .lock()
            .expect("the fake call is not poisoned") = Some(Box::new(hook));
    }

    /// The owner hangs up (or the server ends the call) with `reason`.
    pub fn end(&self, reason: CallEndReason) {
        self.shared.finish(reason);
    }

    /// The owner answers a call the bridge placed.
    pub fn pick_up(&self) {
        self.shared.picked_up.send_replace(true);
        self.shared.changed.notify_waiters();
    }

    /// Whether the owner answered.
    pub fn picked_up(&self) -> bool {
        *self.shared.picked_up.borrow()
    }

    /// Whether the bridge hung up.
    pub fn hung_up(&self) -> bool {
        self.shared.inner().hung_up
    }

    /// How the call ended, if it did.
    pub fn ended(&self) -> Option<LineEnded> {
        self.shared.ended_now()
    }

    /// The call events the bridge gave the line.
    pub fn handled(&self) -> Vec<CallEvent> {
        self.shared.inner().handled.clone()
    }

    /// Waits until `done` holds for the call; panics after 60 s.
    pub async fn wait_until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        wait(&self.shared.changed, what, || done(self)).await;
    }
}

/// The fake's playback: one frame per 20 ms (on absolute deadlines, so coarse timers do
/// not slow it down), skipping frames queued before a `clear()`.
async fn play(shared: Arc<CallShared>, mut played: mpsc::Receiver<(u64, Frame)>) {
    let mut ended = shared.ended.subscribe();
    let mut slot = tokio::time::Instant::now();
    loop {
        let next = tokio::select! {
            next = played.recv() => next,
            _ = ended.wait_for(Option::is_some) => None,
        };
        let Some((generation, frame)) = next else {
            return;
        };
        if generation != shared.generation.load(Ordering::Acquire) {
            continue;
        }
        slot = slot.max(tokio::time::Instant::now());
        {
            let mut inner = shared.inner();
            inner.heard.extend_from_slice(&frame);
            inner.heard_since_clear.extend_from_slice(&frame);
            inner.played_at.push(slot);
        }
        shared.changed.notify_waiters();
        slot += FRAME_TIME;
        tokio::select! {
            () = tokio::time::sleep_until(slot) => {}
            _ = ended.wait_for(Option::is_some) => return,
        }
    }
}

struct FakeSpeaker {
    queue: mpsc::Sender<(u64, Frame)>,
    shared: Arc<CallShared>,
}

#[async_trait::async_trait]
impl Speaker for FakeSpeaker {
    async fn send(&self, frame: Frame) -> bool {
        if self.shared.ended_now().is_some() {
            return false;
        }
        let generation = self.shared.generation.load(Ordering::Acquire);
        self.queue.send((generation, frame)).await.is_ok()
    }

    fn clear(&self) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        {
            let mut inner = self.shared.inner();
            inner.clears += 1;
            inner.heard_since_clear.clear();
        }
        let hook = self
            .shared
            .on_clear
            .lock()
            .expect("the fake call is not poisoned")
            .take();
        if let Some(hook) = hook {
            hook();
        }
        self.shared.changed.notify_waiters();
    }
}

struct FakeControl {
    call_id: CallId,
    shared: Arc<CallShared>,
}

#[async_trait::async_trait]
impl LineControl for FakeControl {
    fn handle(&self, event: &CallEvent) -> bool {
        if !is_about(event, self.call_id) {
            return false;
        }
        self.shared.inner().handled.push(event.clone());
        self.shared.changed.notify_waiters();
        if let CallEvent::Ended {
            reason, duration_s, ..
        } = event
        {
            self.shared.finish_with(LineEnded {
                reason: *reason,
                duration_s: *duration_s,
            });
        }
        true
    }

    async fn connected(&self) -> Result<(), LineEnded> {
        if let Some(end) = self.shared.ended_now() {
            return Err(end);
        }
        let mut picked_up = self.shared.picked_up.subscribe();
        let answered = async move { picked_up.wait_for(|picked_up| *picked_up).await.is_ok() };
        tokio::select! {
            answered = answered => {
                if !answered {
                    return Err(self.shared.wait_ended().await);
                }
            }
            end = self.shared.wait_ended() => return Err(end),
        }
        let Some(delay) = self.shared.connect_delay else {
            return match self.shared.ended_now() {
                Some(end) => Err(end),
                None => Ok(()),
            };
        };
        tokio::select! {
            () = tokio::time::sleep(delay) => match self.shared.ended_now() {
                Some(end) => Err(end),
                None => Ok(()),
            },
            end = self.shared.wait_ended() => Err(end),
        }
    }

    async fn ended(&self) -> LineEnded {
        self.shared.wait_ended().await
    }

    async fn hangup(&self) -> LineEnded {
        self.shared.inner().hung_up = true;
        // Hanging up a call that still rings cancels it.
        let reason = if *self.shared.picked_up.borrow() {
            CallEndReason::Completed
        } else {
            CallEndReason::Cancelled
        };
        self.shared.finish(reason);
        self.shared.wait_ended().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use push_voice_adapter_testkit::tones::{
        SPEAK_AMPLITUDE, SPEAK_HZ, amplitude, tone, tone_frame,
    };

    fn ended(call_id: CallId, reason: CallEndReason) -> CallEvent {
        CallEvent::Ended {
            call_id,
            reason,
            duration_s: Some(7),
        }
    }

    #[tokio::test]
    async fn a_placed_call_rings_until_the_owner_picks_up_or_it_ends() {
        let phone = FakePhone::new();
        let owner = UserId::new();
        let line = phone.call(owner).await.unwrap();
        let call = phone.wait_dialed().await;
        assert_eq!(call.call_id, line.call_id);
        assert_eq!(phone.dialed(), vec![owner]);
        let control = line.control.clone();
        let connecting = tokio::spawn(async move { control.connected().await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!connecting.is_finished(), "rings");
        call.pick_up();
        assert_eq!(connecting.await.unwrap(), Ok(()));

        let missed = phone.call(owner).await.unwrap();
        let call = phone.wait_dialed().await;
        call.end(CallEndReason::Missed);
        assert_eq!(
            missed.control.connected().await.unwrap_err().reason,
            CallEndReason::Missed
        );

        let cancelled = phone.call(owner).await.unwrap();
        let call = phone.wait_dialed().await;
        assert_eq!(
            cancelled.control.hangup().await.reason,
            CallEndReason::Cancelled
        );
        assert!(call.hung_up());

        phone.fail_next_call(SdkError::Offline);
        assert!(matches!(phone.call(owner).await, Err(SdkError::Offline)));
        assert_eq!(phone.dialed().len(), 3);
    }

    #[tokio::test]
    async fn the_fake_phone_answers_declines_and_fails_on_request() {
        let phone = FakePhone::new();
        let first = CallId::new();
        let mut line = phone.answer(first, "offer").await.unwrap();
        assert_eq!(line.call_id, first);
        let call = phone.wait_answered().await;
        assert_eq!(call.call_id, first);
        assert_eq!(phone.answered(), vec![first]);

        call.say(&tone(450.0, 5)).await;
        for _ in 0..5 {
            assert_eq!(line.incoming.recv().await.unwrap().len(), 320);
        }

        phone.fail_next_answer(SdkError::Offline);
        assert!(matches!(
            phone.answer(CallId::new(), "offer").await,
            Err(SdkError::Offline)
        ));
        let declined = CallId::new();
        phone.decline(declined, true).await.unwrap();
        phone.wait_declined(1).await;
        assert_eq!(phone.declined(), vec![(declined, true)]);
    }

    #[tokio::test]
    async fn a_held_answer_waits_for_its_release() {
        let phone = FakePhone::new();
        let release = phone.hold_next_answer();
        let answering = {
            let phone = phone.clone();
            tokio::spawn(async move { phone.answer(CallId::new(), "offer").await.is_ok() })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(phone.answered().is_empty());
        release.send(()).unwrap();
        assert!(answering.await.unwrap());
        assert_eq!(phone.answered().len(), 1);
    }

    #[tokio::test]
    async fn the_speaker_plays_in_real_time_and_clear_drops_the_queue() {
        let phone = FakePhone::new();
        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let call = phone.wait_answered().await;
        let started = Instant::now();
        for index in 0..50 {
            let frame = tone_frame(SPEAK_HZ, SPEAK_AMPLITUDE, index);
            assert!(line.speaker.send(frame).await);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        line.speaker.clear();
        let played = call.heard().len();
        assert!((1600..=8000).contains(&played), "{played}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(call.heard().len(), played);
        assert_eq!(call.clears(), 1);
        assert!(started.elapsed() < Duration::from_millis(900));

        assert!(line.speaker.send([1000; 320]).await);
        call.wait_until("the frame after the clear", |call| {
            call.heard_since_clear().len() == 320
        })
        .await;
        assert!(amplitude(&call.heard()[..1600], SPEAK_HZ) > 5000.0);
    }

    #[tokio::test]
    async fn the_call_ends_by_the_owner_by_event_or_by_hangup() {
        let phone = FakePhone::new();
        let mut line = phone.answer(CallId::new(), "offer").await.unwrap();
        let call = phone.wait_answered().await;
        assert_eq!(line.control.connected().await, Ok(()));
        call.end(CallEndReason::Completed);
        let end = line.control.ended().await;
        assert_eq!(end.reason, CallEndReason::Completed);
        assert!(end.duration_s.is_some());
        assert!(line.incoming.recv().await.is_none());
        assert!(!line.speaker.send([0; 320]).await);
        assert!(!call.hung_up());

        let id = CallId::new();
        let line = phone.answer(id, "offer").await.unwrap();
        let call = phone.wait_answered().await;
        assert!(
            !line
                .control
                .handle(&ended(CallId::new(), CallEndReason::Failed))
        );
        assert!(line.control.handle(&ended(id, CallEndReason::Failed)));
        assert_eq!(
            line.control.ended().await,
            LineEnded {
                reason: CallEndReason::Failed,
                duration_s: Some(7)
            }
        );
        assert_eq!(call.handled().len(), 1);

        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let call = phone.wait_answered().await;
        assert_eq!(line.control.hangup().await.reason, CallEndReason::Completed);
        assert!(call.hung_up());
        assert_eq!(call.ended().unwrap().reason, CallEndReason::Completed);
    }

    #[tokio::test]
    async fn a_delayed_connect_waits_or_reports_the_end() {
        let phone = FakePhone::new();
        phone.delay_connect(Duration::from_millis(100));
        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let started = Instant::now();
        assert_eq!(line.control.connected().await, Ok(()));
        assert!(started.elapsed() >= Duration::from_millis(90));

        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let call = phone.wait_answered().await;
        let connecting = tokio::spawn(async move { line.control.connected().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        call.end(CallEndReason::Cancelled);
        assert_eq!(
            connecting.await.unwrap().unwrap_err().reason,
            CallEndReason::Cancelled
        );
    }
}
