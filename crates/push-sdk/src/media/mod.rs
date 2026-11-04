//! The media helper (feature `calls-media`): a native WebRTC audio peer for a call, for
//! apps without a WebRTC stack of their own (agents, bots, the bridge). See
//! docs/calls.md, "Media helper".
//!
//! [`MediaCall::start`] places or answers a call through [`Calls`] and runs its media on
//! the client's runtime: ICE over UDP with the call's STUN and TURN servers (TURN over UDP
//! or TCP; relay-only when the servers include a relay, as the browser recipe does),
//! DTLS-SRTP (str0m), Opus at 48 kHz mono in 20 ms packets. The app reads and writes
//! 16 kHz mono 16-bit PCM in 20 ms frames ([`PcmFrame`]): [`MediaCall::incoming`] and
//! [`MediaCall::outgoing`]. It forwards the client's call events to
//! [`MediaCall::handle`] and learns the end from [`MediaCall::on_ended`].
//!
//! This PCM (16 kHz, mono, s16le, 20 ms frames of [`FRAME_SAMPLES`] samples) is exactly
//! the audio format of the voice adapter protocol `push-voice-adapter/1`
//! (docs/voice-adapter-protocol.md, "Audio"): push-agent-bridge passes it to and from its
//! voice adapter as is, so an adapter never handles Opus or WebRTC.
//!
//! The feature needs Rust 1.89 (str0m) and a C toolchain with CMake (libopus, AWS-LC);
//! the rest of push-sdk keeps the workspace's Rust 1.85.

mod codec;
mod driver;
mod ice;
mod resample;
mod turn;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use push_domain::{CallId, UserId};
use tokio::sync::{mpsc, watch};

use crate::calls::{CallEndReason, CallErrorCode, CallEvent, IceCandidate, IceServers, SdpKind};
use crate::{Calls, SdkError};

/// The helper's PCM sample rate: 16 kHz, mono, signed 16-bit.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples in one [`PcmFrame`]: 20 ms at 16 kHz.
pub const FRAME_SAMPLES: usize = 320;
/// The duration of one [`PcmFrame`].
pub const FRAME_DURATION: Duration = Duration::from_millis(20);

/// Frames [`PcmSink`] holds before [`PcmSink::send`] waits: 5 s, so synthesised speech can
/// be queued faster than real time.
const OUTGOING_FRAMES: usize = 250;
/// Frames [`PcmStream`] holds for a slow reader before newer ones are dropped: 1 s.
const INCOMING_FRAMES: usize = 50;

/// 20 ms of 16 kHz mono audio. `Debug` shows no samples (audio is never logged).
#[derive(Clone, PartialEq, Eq)]
pub struct PcmFrame(pub [i16; FRAME_SAMPLES]);

impl PcmFrame {
    /// A frame of silence.
    pub fn silence() -> Self {
        Self([0; FRAME_SAMPLES])
    }

    /// The frame of exactly [`FRAME_SAMPLES`] samples; `None` for any other length.
    pub fn from_slice(samples: &[i16]) -> Option<Self> {
        samples.try_into().ok().map(Self)
    }

    pub fn samples(&self) -> &[i16; FRAME_SAMPLES] {
        &self.0
    }
}

impl std::fmt::Debug for PcmFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PcmFrame(20 ms)")
    }
}

/// Who this side of the call is.
#[derive(Clone, PartialEq, Eq)]
pub enum Role {
    /// Calls `peer`: the helper makes the offer and invites.
    Caller { peer: UserId },
    /// Answers a ringing call ([`CallEvent::Incoming`]) with its offer.
    Callee { call_id: CallId, offer_sdp: String },
}

impl std::fmt::Debug for Role {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Caller { peer } => formatter
                .debug_struct("Caller")
                .field("peer", peer)
                .finish(),
            Self::Callee { call_id, offer_sdp } => formatter
                .debug_struct("Callee")
                .field("call_id", call_id)
                .field("offer_sdp", &crate::model::Redacted(offer_sdp.len()))
                .finish(),
        }
    }
}

/// How the helper runs a call. `Default` suits most calls.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MediaOptions {
    /// Relay-only ICE (only the TURN relay candidate; the peer never learns this host's
    /// addresses). `None`: relay-only exactly when the servers include a relay
    /// ([`IceServers::relay`]), as the browser recipe does.
    pub relay_only: Option<bool>,
    /// How long gathering may take (STUN binding, TURN allocation).
    pub gather_timeout: Duration,
    /// How long the media may take to connect once both descriptions are set; then the
    /// helper hangs up as failed.
    pub connect_timeout: Duration,
    /// How long the media may stay disconnected before the helper hangs up as failed.
    pub disconnect_timeout: Duration,
}

impl Default for MediaOptions {
    fn default() -> Self {
        Self {
            relay_only: None,
            gather_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(30),
            disconnect_timeout: Duration::from_secs(15),
        }
    }
}

/// Why the helper's call ended, beyond [`MediaEnded::reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EndCause {
    /// The server ended it ([`CallEvent::Ended`]): a hang-up, decline, cancel, miss, …
    Server,
    /// [`MediaCall::hangup`], or the `MediaCall` was dropped.
    Local,
    /// The media never connected or broke for too long: the helper hung up as failed.
    MediaFailed,
    /// After a reconnect the server no longer lists the call ([`CallEvent::Synced`]).
    Gone,
    /// The server refused this side's frame for good (an invite to a non-contact, an
    /// answer after another device's).
    Refused(CallErrorCode),
}

/// How a call ended for the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaEnded {
    /// The server's reason with [`EndCause::Server`]; `Completed` for a local hang-up and
    /// `Failed` otherwise.
    pub reason: CallEndReason,
    /// Seconds from the answer to the end, when the server says so.
    pub duration_s: Option<u32>,
    pub cause: EndCause,
}

/// Where the helper's call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    /// Ringing, or ICE and DTLS are being established.
    Connecting,
    /// Audio flows.
    Connected,
    Ended(MediaEnded),
}

/// The audio the peer sends, decoded to [`PcmFrame`]s. Ends (`None`) when the call ends.
/// A reader more than 1 s behind loses the newest frames.
#[derive(Debug)]
pub struct PcmStream {
    receiver: mpsc::Receiver<PcmFrame>,
}

impl PcmStream {
    /// The next frame; `None` once the call ended.
    pub async fn recv(&mut self) -> Option<PcmFrame> {
        self.receiver.recv().await
    }

    /// The next frame if one is waiting.
    pub fn try_recv(&mut self) -> Option<PcmFrame> {
        self.receiver.try_recv().ok()
    }
}

impl futures_util::Stream for PcmStream {
    type Item = PcmFrame;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<PcmFrame>> {
        self.receiver.poll_recv(context)
    }
}

/// Where the app writes the audio it sends. The helper sends one frame per 20 ms once
/// connected, so frames may be queued faster than real time (up to 5 s); nothing is sent
/// while the queue is empty. Cheap to clone.
#[derive(Debug, Clone)]
pub struct PcmSink {
    sender: mpsc::Sender<(u64, PcmFrame)>,
    generation: Arc<AtomicU64>,
}

impl PcmSink {
    /// Queues `frame`, waiting while the queue is full. `false` once the call ended.
    pub async fn send(&self, frame: PcmFrame) -> bool {
        let generation = self.generation.load(Ordering::Acquire);
        self.sender.send((generation, frame)).await.is_ok()
    }

    /// Queues `frame` unless the queue is full or the call ended.
    pub fn try_send(&self, frame: PcmFrame) -> bool {
        let generation = self.generation.load(Ordering::Acquire);
        self.sender.try_send((generation, frame)).is_ok()
    }

    /// Drops every queued frame not sent yet (barge-in: stop speaking at once).
    pub fn clear(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// What the app tells the driver.
#[derive(Debug)]
enum Command {
    Event(CallEvent),
    Hangup,
}

/// The signalling the driver needs: [`Calls`] in an app, a direct wire in tests.
pub(crate) trait Signalling: Clone + Send + Sync + 'static {
    fn invite(
        &self,
        peer: UserId,
        offer_sdp: String,
    ) -> impl Future<Output = Result<CallId, SdkError>> + Send;
    fn accept(
        &self,
        call: CallId,
        answer_sdp: String,
    ) -> impl Future<Output = Result<(), SdkError>> + Send;
    fn send_ice(
        &self,
        call: CallId,
        candidate: IceCandidate,
    ) -> impl Future<Output = Result<(), SdkError>> + Send;
    fn renegotiate(
        &self,
        call: CallId,
        sdp: String,
        kind: SdpKind,
    ) -> impl Future<Output = Result<(), SdkError>> + Send;
    fn hangup(
        &self,
        call: CallId,
        failed: bool,
    ) -> impl Future<Output = Result<(), SdkError>> + Send;
}

impl Signalling for Calls {
    async fn invite(&self, peer: UserId, offer_sdp: String) -> Result<CallId, SdkError> {
        Calls::invite(self, peer, &offer_sdp).await
    }

    async fn accept(&self, call: CallId, answer_sdp: String) -> Result<(), SdkError> {
        Calls::accept(self, call, &answer_sdp).await
    }

    async fn send_ice(&self, call: CallId, candidate: IceCandidate) -> Result<(), SdkError> {
        Calls::send_ice(self, call, candidate).await
    }

    async fn renegotiate(&self, call: CallId, sdp: String, kind: SdpKind) -> Result<(), SdkError> {
        Calls::renegotiate(self, call, &sdp, kind).await
    }

    async fn hangup(&self, call: CallId, failed: bool) -> Result<(), SdkError> {
        Calls::hangup(self, call, failed).await
    }
}

/// One call's media (feature `calls-media`). Dropping it hangs up a call still running.
///
/// The driver holds a clone of the client until the call ends: end calls before
/// [`Client::close`](crate::Client::close).
#[derive(Debug)]
pub struct MediaCall {
    call_id: CallId,
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<MediaState>,
    incoming: Option<PcmStream>,
    sink: PcmSink,
}

impl MediaCall {
    /// Starts a call's media with `servers` (fetch them per call:
    /// [`Calls::ice_servers`]). As [`Role::Caller`] it gathers candidates, invites with
    /// its offer and returns (the call rings); as [`Role::Callee`] it applies the offer and
    /// accepts. From then on forward every [`CallEvent`] to [`MediaCall::handle`],
    /// including those that arrived while this ran (the answer may): read the client's
    /// events on the task that awaits this, or keep the call's events for it. A caller
    /// whose answer never reaches the helper hangs up as failed after 90 s plus
    /// [`MediaOptions::connect_timeout`].
    ///
    /// Errors: the input checks and refusals of [`Calls::invite`] and [`Calls::accept`]
    /// (`offline`, `not_available`, `invalid`); [`SdkError::Invalid`] for an offer it
    /// cannot answer (no Opus audio); [`SdkError::Network`] when no candidate could be
    /// gathered (relay-only without a TURN allocation, say).
    pub async fn start(
        calls: &Calls,
        servers: &IceServers,
        role: Role,
        options: MediaOptions,
    ) -> Result<Self, SdkError> {
        let signalling = calls.clone();
        let servers = servers.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // Not cut short by a caller that gives up: once invited or accepted, the call
        // must be hung up, which the dropped `MediaCall` then does.
        calls.client().spawn(async move {
            let started = Self::start_with(signalling, servers, role, options).await;
            let _ = sender.send(started);
        })?;
        receiver.await.unwrap_or(Err(SdkError::Closed))
    }

    /// [`MediaCall::start`] over any signalling, on the current runtime.
    async fn start_with<S: Signalling>(
        signalling: S,
        servers: IceServers,
        role: Role,
        options: MediaOptions,
    ) -> Result<Self, SdkError> {
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let (state_sender, state) = watch::channel(MediaState::Connecting);
        let (incoming_sender, incoming) = mpsc::channel(INCOMING_FRAMES);
        let (outgoing_sender, outgoing) = mpsc::channel(OUTGOING_FRAMES);
        let generation = Arc::new(AtomicU64::new(0));
        let channels = driver::Channels {
            commands: command_receiver,
            state: state_sender,
            incoming: incoming_sender,
            outgoing,
            generation: generation.clone(),
        };
        let call_id = driver::start(signalling, &servers, role, &options, channels).await?;
        Ok(Self {
            call_id,
            commands,
            state,
            incoming: Some(PcmStream { receiver: incoming }),
            sink: PcmSink {
                sender: outgoing_sender,
                generation,
            },
        })
    }

    /// The call's id.
    pub fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Gives the helper a call event; `true` when it was about this call. Forward every
    /// [`CallEvent`] (the answer, the peer's candidates and renegotiations, the end, and
    /// `Synced` after a reconnect).
    pub fn handle(&self, event: &CallEvent) -> bool {
        let ours = match event {
            CallEvent::Incoming { call_id, .. }
            | CallEvent::Accepted { call_id, .. }
            | CallEvent::Taken { call_id }
            | CallEvent::Ended { call_id, .. }
            | CallEvent::PeerIce { call_id, .. }
            | CallEvent::PeerRenegotiate { call_id, .. }
            | CallEvent::Error { call_id, .. } => *call_id == self.call_id,
            CallEvent::Synced { calls, gone } => {
                gone.contains(&self.call_id)
                    || calls.iter().any(|call| call.call_id == self.call_id)
            }
            CallEvent::HistoryChanged { .. } => false,
        };
        if ours {
            let _ = self.commands.send(Command::Event(event.clone()));
        }
        ours
    }

    /// The peer's audio. The first call takes the stream; later ones get `None`.
    pub fn incoming(&mut self) -> Option<PcmStream> {
        self.incoming.take()
    }

    /// Where to write this side's audio.
    pub fn outgoing(&self) -> PcmSink {
        self.sink.clone()
    }

    /// Where the call stands now.
    pub fn state(&self) -> MediaState {
        *self.state.borrow()
    }

    /// Waits until audio flows; `Err` with the end if the call ends first.
    pub async fn connected(&self) -> Result<(), MediaEnded> {
        let mut state = self.state.clone();
        loop {
            match *state.borrow_and_update() {
                MediaState::Connected => return Ok(()),
                MediaState::Ended(ended) => return Err(ended),
                MediaState::Connecting => {}
            }
            if state.changed().await.is_err() {
                return Err(dropped());
            }
        }
    }

    /// Waits for the end of the call.
    pub async fn on_ended(&self) -> MediaEnded {
        let mut state = self.state.clone();
        loop {
            if let MediaState::Ended(ended) = *state.borrow_and_update() {
                return ended;
            }
            if state.changed().await.is_err() {
                return dropped();
            }
        }
    }

    /// Hangs up (completed) and stops the media; returns once the call ended (the TURN
    /// allocation is released right after).
    pub async fn hangup(&self) -> MediaEnded {
        let _ = self.commands.send(Command::Hangup);
        self.on_ended().await
    }
}

impl Drop for MediaCall {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Hangup);
    }
}

/// The driver went away without saying how (its runtime stopped).
fn dropped() -> MediaEnded {
    MediaEnded {
        reason: CallEndReason::Failed,
        duration_s: None,
        cause: EndCause::Local,
    }
}
