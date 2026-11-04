//! One connection to a voice adapter (push-voice-adapter/1, design §3.4–3.5, A6, A7,
//! A11–A13): the handshake, liveness, the caps on what the adapter sends, and the bridge's
//! send queues.
//!
//! [`Link::open`] sends the bridge's hello and waits for the adapter's; then two tasks run
//! the connection:
//!
//! - the **reader** decodes frames, answers pings, enforces the rate caps and delivers
//!   [`LinkEvent`]s: validated messages (never hello, ping or pong) and well-formed
//!   stream-1 audio, then exactly one [`LinkEvent::Closed`];
//! - the **writer** sends control messages first (never dropped), then a ping every
//!   [`PING_INTERVAL`], then caller audio from a queue of [`TO_ADAPTER_QUEUE_FRAMES`]
//!   frames that drops the oldest when full.
//!
//! [`Link`] is a cheap handle (`Clone`, an `Arc` inside) for the router and the
//! supervisor. How the link ends is recorded once, in a `watch` channel: the router learns
//! it from [`LinkEvent::Closed`], the supervisor from [`Link::closed`] (a [`LinkClosed`]
//! that does not keep the link open), so the supervisor can hand the event receiver to the
//! router and still learn when to restart. [`Link::close`] and dropping every `Link` end
//! the link with [`LinkEnd::Shutdown`]: queued control messages are flushed (at most
//! [`CLOSE_GRACE`]), then the writer is shut down.
//!
//! The event channel is unbounded so [`LinkEvent::Closed`] can never be lost or block the
//! reader. It is bounded in practice by the reader's caps: at most
//! [`MAX_CONTROL_PER_SECOND`] messages a second (more ends the link) and stream-1 audio at
//! most [`AUDIO_RATE_FACTOR`] times real time, only during a call. Its consumer must
//! always drain it: the router during a call, otherwise the supervisor (which drains it
//! every second when no call runs).
//!
//! Content-free (A15): logs carry type names, codes, ids and counts, never text or audio.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use push_voice_adapter_proto::limits::{
    DEAD_PEER_AFTER, HELLO_TIMEOUT, MAX_CONTROL_PER_SECOND, MAX_PARTIALS_PER_SECOND, PING_INTERVAL,
    TO_ADAPTER_QUEUE_FRAMES,
};
use push_voice_adapter_proto::{
    AdapterHello, AdapterMessage, AsyncFrameReader, AsyncFrameWriter, AudioFormat, AudioFrame,
    BridgeHello, BridgeMessage, Capabilities, Decoded, ErrorCode, ErrorMessage, Frame,
    MessageError, Ping, Pong, ProtocolVersion, ReadError, Stream, Voice, decode, encode,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout, timeout_at};

use crate::voice::FRAME;

/// How long the writer may take to flush queued control messages and shut down once the
/// link is ending.
pub const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Stream-1 audio faster than this many times real time over [`AUDIO_RATE_WINDOW`] is
/// dropped (A13).
pub const AUDIO_RATE_FACTOR: u32 = 4;
/// The window of the stream-1 rate cap (A13).
pub const AUDIO_RATE_WINDOW: Duration = Duration::from_secs(2);

/// What the adapter said about itself in its hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// The negotiated version (one the bridge offered).
    pub protocol: String,
    pub name: String,
    pub version: String,
    pub capabilities: Capabilities,
    pub voices: Vec<Voice>,
}

impl From<AdapterHello> for AdapterInfo {
    fn from(hello: AdapterHello) -> Self {
        Self {
            protocol: hello.protocol,
            name: hello.name,
            version: hello.version,
            capabilities: hello.capabilities,
            voices: hello.voices,
        }
    }
}

/// What the link delivers.
#[derive(Debug)]
pub enum LinkEvent {
    /// A validated message; never `hello`, `ping` or `pong`.
    Message(AdapterMessage),
    /// Speech for the caller: stream 1, the negotiated frame size, during a call, within
    /// the rate cap.
    Audio(AudioFrame),
    /// The link ended; the last event, delivered exactly once.
    Closed(LinkEnd),
}

/// Why a link ended (or never opened). Content-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEnd {
    /// The adapter closed its side.
    Eof,
    /// Nothing received, or nothing could be written, for [`DEAD_PEER_AFTER`].
    DeadPeer,
    /// The adapter sent more than [`MAX_CONTROL_PER_SECOND`] control messages in a second.
    Overloaded,
    /// A framing error in what the adapter sent.
    Framing(String),
    /// The handshake failed: no hello in time, a version not offered, a refusal.
    Protocol(String),
    /// Reading or writing the stream failed.
    Io(String),
    /// The bridge closed the link ([`Link::close`], or every [`Link`] dropped).
    Shutdown,
}

impl fmt::Display for LinkEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkEnd::Eof => f.write_str("the voice adapter closed the connection"),
            LinkEnd::DeadPeer => f.write_str("the voice adapter stopped responding"),
            LinkEnd::Overloaded => f.write_str("the voice adapter sent too many messages"),
            LinkEnd::Framing(error) => write!(f, "the voice adapter sent a bad frame: {error}"),
            LinkEnd::Protocol(error) | LinkEnd::Io(error) => f.write_str(error),
            LinkEnd::Shutdown => f.write_str("the bridge closed the connection"),
        }
    }
}

/// Counters of a link, for metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkStats {
    /// Frames received (control and audio).
    pub frames_in: u64,
    /// Frames written (control and audio).
    pub frames_out: u64,
    /// Stream-1 frames delivered as [`LinkEvent::Audio`].
    pub audio_in: u64,
    /// Stream-0 frames written.
    pub audio_out: u64,
    /// Caller audio dropped because the send queue was full.
    pub dropped_to_adapter: u64,
    /// Audio from the adapter dropped: wrong stream or size, no call, over the rate cap.
    pub dropped_from_adapter: u64,
    /// Partial transcripts dropped over [`MAX_PARTIALS_PER_SECOND`].
    pub partials_dropped: u64,
    /// Malformed control messages answered with `invalid_message`.
    pub invalid_messages: u64,
}

#[derive(Default)]
struct Counters {
    frames_in: AtomicU64,
    frames_out: AtomicU64,
    audio_in: AtomicU64,
    audio_out: AtomicU64,
    dropped_to_adapter: AtomicU64,
    dropped_from_adapter: AtomicU64,
    partials_dropped: AtomicU64,
    invalid_messages: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    fn snapshot(&self) -> LinkStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        LinkStats {
            frames_in: get(&self.frames_in),
            frames_out: get(&self.frames_out),
            audio_in: get(&self.audio_in),
            audio_out: get(&self.audio_out),
            dropped_to_adapter: get(&self.dropped_to_adapter),
            dropped_from_adapter: get(&self.dropped_from_adapter),
            partials_dropped: get(&self.partials_dropped),
            invalid_messages: get(&self.invalid_messages),
        }
    }
}

/// What the writer sends besides audio.
enum Outgoing {
    Control(Frame),
    /// Flush what is queued before this, then shut the writer down.
    Close,
}

/// The current call and its stream-0 queue.
#[derive(Default)]
struct CallState {
    call_id: Option<String>,
    seq: u32,
    timestamp_ms: u32,
    /// One `unsupported_format` per call.
    format_error_sent: bool,
    queue: VecDeque<AudioFrame>,
}

/// State shared by the handles and both tasks.
struct Shared {
    format: AudioFormat,
    call: Mutex<CallState>,
    audio_ready: Notify,
    end: watch::Sender<Option<LinkEnd>>,
    stats: Counters,
}

impl Shared {
    fn call(&self) -> MutexGuard<'_, CallState> {
        self.call.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records how the link ended; only the first end counts.
    fn finish(&self, end: LinkEnd) {
        self.end.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(end);
            true
        });
    }

    fn has_audio(&self) -> bool {
        !self.call().queue.is_empty()
    }

    fn pop_audio(&self) -> Option<AudioFrame> {
        self.call().queue.pop_front()
    }
}

/// The handles' common part; dropping the last one closes the link.
struct Handle {
    control: mpsc::UnboundedSender<Outgoing>,
    shared: Arc<Shared>,
    info: AdapterInfo,
}

impl Handle {
    fn close(&self) {
        self.shared.finish(LinkEnd::Shutdown);
        let _ = self.control.send(Outgoing::Close);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.close();
    }
}

/// An open link to a voice adapter. Cloning is cheap; the link closes when
/// [`Link::close`] is called or every clone is dropped.
#[derive(Clone)]
pub struct Link {
    handle: Arc<Handle>,
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("adapter", &self.handle.info.name)
            .field("end", &*self.handle.shared.end.borrow())
            .finish()
    }
}

/// Resolves when a link ended, without keeping it open (see [`Link::closed`]).
#[derive(Debug, Clone)]
pub struct LinkClosed(watch::Receiver<Option<LinkEnd>>);

impl LinkClosed {
    /// How the link ended, once it has.
    pub async fn wait(&mut self) -> LinkEnd {
        if let Ok(end) = self.0.wait_for(Option::is_some).await {
            if let Some(end) = end.clone() {
                return end;
            }
        }
        self.get().unwrap_or(LinkEnd::Shutdown)
    }

    /// How the link ended, if it has.
    pub fn get(&self) -> Option<LinkEnd> {
        self.0.borrow().clone()
    }
}

impl Link {
    /// Sends `hello`, waits for the adapter's (the whole exchange within
    /// [`HELLO_TIMEOUT`]) and starts the
    /// link's tasks (call it inside a tokio runtime). A failed handshake answers the
    /// adapter with an `error` when the protocol says so, shuts the writer down and
    /// returns why.
    pub async fn open<R, W>(
        reader: R,
        writer: W,
        hello: BridgeHello,
    ) -> Result<(Link, mpsc::UnboundedReceiver<LinkEvent>), LinkEnd>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let format = AudioFormat::from_spec(&hello.audio).unwrap_or(AudioFormat::V1);
        let offered: Vec<String> = hello.offered().into_iter().map(str::to_owned).collect();
        let hello = encode(&BridgeMessage::Hello(hello)).map_err(|error| {
            LinkEnd::Protocol(format!("the bridge's hello is invalid: {error}"))
        })?;
        let mut reader = AsyncFrameReader::new(reader);
        let mut writer = AsyncFrameWriter::new(writer);
        let deadline = Instant::now() + HELLO_TIMEOUT;
        match timeout_at(deadline, writer.write_frame(&hello)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(LinkEnd::Io(error.to_string())),
            Err(_) => return Err(no_hello()),
        }
        let answer = timeout_at(deadline, await_hello(&mut reader, &offered, format))
            .await
            .unwrap_or_else(|_| Err(Refusal::quiet(no_hello())));
        let adapter = match answer {
            Ok(adapter) => adapter,
            Err(Refusal { reply, end }) => {
                if let Some(reply) = reply {
                    let _ = timeout(
                        CLOSE_GRACE,
                        writer.write_message(&BridgeMessage::Error(*reply)),
                    )
                    .await;
                }
                let _ = timeout(CLOSE_GRACE, writer.shutdown()).await;
                tracing::info!(event = "adapter_handshake_failed", reason = %end);
                return Err(end);
            }
        };
        let info = AdapterInfo::from(adapter);
        tracing::info!(
            event = "adapter_ready",
            name = %info.name,
            version = %info.version,
            stt = info.capabilities.stt,
            tts = info.capabilities.tts,
            vad = info.capabilities.vad,
        );

        let shared = Arc::new(Shared {
            format,
            call: Mutex::new(CallState::default()),
            audio_ready: Notify::new(),
            end: watch::channel(None).0,
            stats: Counters::default(),
        });
        let (control, control_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let reading = Reader {
            reader,
            control: control.clone(),
            events,
            shared: Arc::clone(&shared),
            controls: Window::new(Duration::from_secs(1), MAX_CONTROL_PER_SECOND as usize),
            partials: Window::new(Duration::from_secs(1), MAX_PARTIALS_PER_SECOND as usize),
            audio: Window::new(
                AUDIO_RATE_WINDOW,
                (AUDIO_RATE_FACTOR as u128 * AUDIO_RATE_WINDOW.as_millis()
                    / format.frame_ms.max(1) as u128) as usize,
            ),
            unknown_types: HashSet::new(),
        };
        tokio::spawn(reading.run());
        tokio::spawn(write_loop(writer, control_rx, Arc::clone(&shared)));
        let link = Link {
            handle: Arc::new(Handle {
                control,
                shared,
                info,
            }),
        };
        Ok((link, events_rx))
    }

    /// Queues a control message; control messages are never dropped and go before audio.
    /// `call_started` starts a call (stream-0 `seq` and timestamps restart at 0) and
    /// `call_ended` ends it (queued audio is dropped; no audio is sent without a call).
    /// After the link closed the message is discarded. Fails only for a message that does
    /// not validate.
    pub fn send(&self, message: BridgeMessage) -> Result<(), MessageError> {
        let frame = encode(&message)?;
        let mut call = self.handle.shared.call();
        match &message {
            BridgeMessage::CallStarted(started) => {
                *call = CallState {
                    call_id: Some(started.call_id.clone()),
                    ..CallState::default()
                };
            }
            BridgeMessage::CallEnded(_) => *call = CallState::default(),
            _ => {}
        }
        let _ = self.handle.control.send(Outgoing::Control(frame));
        Ok(())
    }

    /// Queues one frame of the caller's audio (stream 0) when a call is active. The queue
    /// holds [`TO_ADAPTER_QUEUE_FRAMES`]; when full the oldest frame is dropped (the `seq`
    /// gap tells the adapter).
    pub fn send_audio(&self, samples: &[i16; FRAME]) {
        let shared = &self.handle.shared;
        {
            let mut call = shared.call();
            if call.call_id.is_none() {
                return;
            }
            let frame = AudioFrame {
                stream: Stream::FromCaller,
                seq: call.seq,
                timestamp_ms: call.timestamp_ms,
                samples: samples.to_vec(),
            };
            call.seq = call.seq.wrapping_add(1);
            call.timestamp_ms = call.timestamp_ms.wrapping_add(shared.format.frame_ms);
            if call.queue.len() >= TO_ADAPTER_QUEUE_FRAMES {
                call.queue.pop_front();
                bump(&shared.stats.dropped_to_adapter);
            }
            call.queue.push_back(frame);
        }
        shared.audio_ready.notify_one();
    }

    /// What the adapter said in its hello.
    pub fn info(&self) -> &AdapterInfo {
        &self.handle.info
    }

    /// The link's counters.
    pub fn stats(&self) -> LinkStats {
        self.handle.shared.stats.snapshot()
    }

    /// Closes the link ([`LinkEnd::Shutdown`]): no more events; queued control messages
    /// are flushed, then the writer is shut down.
    pub fn close(&self) {
        self.handle.close();
    }

    /// How the link ended, if it has.
    pub fn end(&self) -> Option<LinkEnd> {
        self.handle.shared.end.borrow().clone()
    }

    /// A signal of the link's end that does not keep the link open.
    pub fn closed(&self) -> LinkClosed {
        LinkClosed(self.handle.shared.end.subscribe())
    }
}

fn no_hello() -> LinkEnd {
    LinkEnd::Protocol(format!(
        "the voice adapter sent no hello within {} s",
        HELLO_TIMEOUT.as_secs()
    ))
}

/// A failed handshake: the error to answer with, if any, and why.
struct Refusal {
    reply: Option<Box<ErrorMessage>>,
    end: LinkEnd,
}

impl Refusal {
    fn quiet(end: LinkEnd) -> Self {
        Self { reply: None, end }
    }

    fn reply(code: ErrorCode, message: impl Into<String>, end: LinkEnd) -> Self {
        Self {
            reply: Some(Box::new(ErrorMessage::new(code, message))),
            end,
        }
    }
}

/// Reads until the adapter's hello. Unknown types are skipped; anything else first is
/// fatal.
async fn await_hello<R: AsyncRead + Unpin>(
    reader: &mut AsyncFrameReader<R>,
    offered: &[String],
    format: AudioFormat,
) -> Result<AdapterHello, Refusal> {
    let ours = offered.join(", ");
    let mut unknown_types = HashSet::new();
    loop {
        let json = match reader.read_frame().await {
            Ok(Some(Frame::Control(json))) => json,
            Ok(Some(Frame::Audio(_))) => {
                return Err(Refusal::reply(
                    ErrorCode::InvalidMessage,
                    "audio before the hello",
                    LinkEnd::Protocol("the voice adapter sent audio before its hello".into()),
                ));
            }
            Ok(None) => return Err(Refusal::quiet(LinkEnd::Eof)),
            Err(ReadError::Frame(error)) => {
                return Err(Refusal::reply(
                    ErrorCode::InvalidFrame,
                    error.to_string(),
                    LinkEnd::Framing(error.to_string()),
                ));
            }
            Err(error) => return Err(Refusal::quiet(LinkEnd::Io(error.to_string()))),
        };
        match decode::<AdapterMessage>(&json) {
            Ok(Decoded::Known(AdapterMessage::Hello(hello))) => {
                if offered.contains(&hello.protocol) {
                    return Ok(hello);
                }
                let theirs = ProtocolVersion::parse(&hello.protocol)
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "an unknown protocol".to_owned());
                return Err(Refusal::reply(
                    ErrorCode::UnsupportedProtocol,
                    format!("this bridge speaks {ours}"),
                    LinkEnd::Protocol(format!(
                        "voice adapter speaks {theirs}; this bridge speaks {ours}"
                    )),
                ));
            }
            Ok(Decoded::Known(AdapterMessage::Error(error))) => {
                return Err(Refusal::quiet(LinkEnd::Protocol(refusal_text(
                    error.code, &ours, format,
                ))));
            }
            Ok(Decoded::Known(other)) => {
                let message_type = push_voice_adapter_proto::message::Message::message_type(&other);
                return Err(Refusal::reply(
                    ErrorCode::InvalidMessage,
                    "expected hello",
                    LinkEnd::Protocol(format!(
                        "the voice adapter sent `{message_type}` before its hello"
                    )),
                ));
            }
            Ok(Decoded::Unknown(name)) => log_unknown(&mut unknown_types, name),
            Err(error) => {
                return Err(Refusal::reply(
                    ErrorCode::InvalidMessage,
                    error.to_string(),
                    LinkEnd::Protocol(format!("the voice adapter's hello is invalid: {error}")),
                ));
            }
        }
    }
}

/// The clear line for an adapter that answered the hello with an error.
fn refusal_text(code: ErrorCode, ours: &str, format: AudioFormat) -> String {
    match code {
        ErrorCode::UnsupportedProtocol => {
            format!("voice adapter does not speak {ours} (unsupported_protocol)")
        }
        ErrorCode::UnsupportedFormat => format!(
            "voice adapter cannot take the bridge's audio ({} Hz, {} channel(s), s16le, {} ms \
             frames) (unsupported_format)",
            format.rate, format.channels, format.frame_ms
        ),
        code => format!("voice adapter refused the hello ({code})"),
    }
}

fn log_unknown(seen: &mut HashSet<String>, name: String) {
    if seen.len() < 64 && !seen.contains(&name) {
        tracing::debug!(event = "adapter_unknown_type", name = %name);
        seen.insert(name);
    }
}

/// A sliding window admitting at most `max` events per `span`.
struct Window {
    span: Duration,
    max: usize,
    times: VecDeque<Instant>,
}

impl Window {
    fn new(span: Duration, max: usize) -> Self {
        Self {
            span,
            max,
            times: VecDeque::with_capacity(max + 1),
        }
    }

    fn admit(&mut self, now: Instant) -> bool {
        while self
            .times
            .front()
            .is_some_and(|first| now.duration_since(*first) >= self.span)
        {
            self.times.pop_front();
        }
        if self.times.len() >= self.max {
            return false;
        }
        self.times.push_back(now);
        true
    }
}

struct Reader<R> {
    reader: AsyncFrameReader<R>,
    control: mpsc::UnboundedSender<Outgoing>,
    events: mpsc::UnboundedSender<LinkEvent>,
    shared: Arc<Shared>,
    controls: Window,
    partials: Window,
    audio: Window,
    unknown_types: HashSet<String>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    async fn run(mut self) {
        let mut ending = self.shared.end.subscribe();
        let mut last_frame = Instant::now();
        loop {
            if ending.borrow_and_update().is_some() {
                break;
            }
            tokio::select! {
                biased;
                _ = ending.changed() => {}
                () = sleep_until(last_frame + DEAD_PEER_AFTER) => {
                    self.fail(None, LinkEnd::DeadPeer);
                }
                frame = self.reader.read_frame() => {
                    last_frame = Instant::now();
                    match frame {
                        Ok(Some(frame)) => {
                            bump(&self.shared.stats.frames_in);
                            if let Err(end) = self.on_frame(frame) {
                                self.fail(None, end);
                            }
                        }
                        Ok(None) => self.fail(None, LinkEnd::Eof),
                        Err(ReadError::Frame(error)) => self.fail(
                            Some(ErrorMessage::new(ErrorCode::InvalidFrame, error.to_string())),
                            LinkEnd::Framing(error.to_string()),
                        ),
                        Err(error) => self.fail(None, LinkEnd::Io(error.to_string())),
                    }
                }
            }
        }
        let end = self
            .shared
            .end
            .borrow()
            .clone()
            .unwrap_or(LinkEnd::Shutdown);
        tracing::info!(event = "adapter_link_closed", reason = %end);
        let _ = self.events.send(LinkEvent::Closed(end));
    }

    /// Ends the link: the error (if any) is queued for the writer to flush, then the end
    /// is recorded, then the writer is told to close — in that order, so the writer can
    /// never record its own end first.
    fn fail(&self, reply: Option<ErrorMessage>, end: LinkEnd) {
        if let Some(reply) = reply {
            self.reply(reply);
        }
        self.shared.finish(end);
        let _ = self.control.send(Outgoing::Close);
    }

    fn reply(&self, message: ErrorMessage) {
        self.send(BridgeMessage::Error(message));
    }

    fn send(&self, message: BridgeMessage) {
        match encode(&message) {
            Ok(frame) => {
                let _ = self.control.send(Outgoing::Control(frame));
            }
            Err(error) => tracing::warn!(event = "adapter_reply_invalid", error = %error),
        }
    }

    /// `Err` ends the link; its error reply is already queued.
    fn on_frame(&mut self, frame: Frame) -> Result<(), LinkEnd> {
        match frame {
            Frame::Control(json) => self.on_control(&json),
            Frame::Audio(audio) => {
                self.on_audio(audio);
                Ok(())
            }
        }
    }

    fn on_control(&mut self, json: &[u8]) -> Result<(), LinkEnd> {
        let now = Instant::now();
        let decoded = decode::<AdapterMessage>(json);
        if let Ok(Decoded::Known(AdapterMessage::Transcript(transcript))) = &decoded {
            if !transcript.is_final && !self.partials.admit(now) {
                bump(&self.shared.stats.partials_dropped);
                return Ok(());
            }
        }
        if !self.controls.admit(now) {
            tracing::warn!(event = "adapter_overloaded");
            self.reply(ErrorMessage::new(
                ErrorCode::Overloaded,
                format!("more than {MAX_CONTROL_PER_SECOND} control messages in a second"),
            ));
            return Err(LinkEnd::Overloaded);
        }
        match decoded {
            Ok(Decoded::Known(message)) => self.on_message(message),
            Ok(Decoded::Unknown(name)) => log_unknown(&mut self.unknown_types, name),
            Err(error) => {
                bump(&self.shared.stats.invalid_messages);
                tracing::debug!(event = "adapter_invalid_message", error = %error);
                self.reply(ErrorMessage::new(
                    ErrorCode::InvalidMessage,
                    error.to_string(),
                ));
            }
        }
        Ok(())
    }

    fn on_message(&mut self, message: AdapterMessage) {
        match message {
            AdapterMessage::Hello(_) => tracing::debug!(event = "adapter_repeated_hello"),
            AdapterMessage::Ping(Ping { id }) => self.send(BridgeMessage::Pong(Pong { id })),
            AdapterMessage::Pong(_) => {}
            message => {
                if let AdapterMessage::Error(error) = &message {
                    tracing::debug!(event = "adapter_error", code = %error.code);
                }
                let _ = self.events.send(LinkEvent::Message(message));
            }
        }
    }

    fn on_audio(&mut self, audio: AudioFrame) {
        let format_ok =
            audio.stream == Stream::ToCaller && self.shared.format.check(&audio).is_ok();
        let in_call = {
            let mut call = self.shared.call();
            if call.call_id.is_some() && !format_ok && !call.format_error_sent {
                call.format_error_sent = true;
                let call_id = call.call_id.clone().unwrap_or_default();
                drop(call);
                tracing::debug!(event = "adapter_audio_format", call_id = %call_id);
                self.reply(
                    ErrorMessage::new(
                        ErrorCode::UnsupportedFormat,
                        format!(
                            "audio from the adapter must be stream 1 with {} samples per frame",
                            self.shared.format.samples_per_frame()
                        ),
                    )
                    .for_call(call_id),
                );
                false
            } else {
                call.call_id.is_some()
            }
        };
        if !in_call || !format_ok || !self.audio.admit(Instant::now()) {
            bump(&self.shared.stats.dropped_from_adapter);
            return;
        }
        bump(&self.shared.stats.audio_in);
        let _ = self.events.send(LinkEvent::Audio(audio));
    }
}

/// The writer's stream and when it must be done once the link is ending.
struct Out<W> {
    writer: AsyncFrameWriter<W>,
    ending: watch::Receiver<Option<LinkEnd>>,
    grace_until: Option<Instant>,
}

impl<W: AsyncWrite + Unpin> Out<W> {
    /// Whether the link is ending; starts the grace period the first time.
    fn closing(&mut self) -> bool {
        if self.grace_until.is_none() && self.ending.borrow_and_update().is_some() {
            self.grace_until = Some(Instant::now() + CLOSE_GRACE);
        }
        self.grace_until.is_some()
    }

    /// Writes one frame within [`DEAD_PEER_AFTER`], or within the grace period once the
    /// link is ending.
    async fn write(&mut self, frame: &Frame) -> Result<(), LinkEnd> {
        let mut deadline = self
            .grace_until
            .unwrap_or_else(|| Instant::now() + DEAD_PEER_AFTER);
        let Out {
            writer,
            ending,
            grace_until,
        } = self;
        let write = writer.write_frame(frame);
        tokio::pin!(write);
        loop {
            tokio::select! {
                result = &mut write => return result.map_err(|error| LinkEnd::Io(error.to_string())),
                () = sleep_until(deadline) => return Err(LinkEnd::DeadPeer),
                changed = ending.changed(), if grace_until.is_none() => {
                    if changed.is_err() || ending.borrow_and_update().is_some() {
                        let until = Instant::now() + CLOSE_GRACE;
                        *grace_until = Some(until);
                        deadline = deadline.min(until);
                    }
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        let deadline = self
            .grace_until
            .unwrap_or_else(|| Instant::now() + CLOSE_GRACE);
        let _ = timeout_at(deadline, self.writer.shutdown()).await;
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    writer: AsyncFrameWriter<W>,
    mut control: mpsc::UnboundedReceiver<Outgoing>,
    shared: Arc<Shared>,
) {
    let mut out = Out {
        ending: shared.end.subscribe(),
        writer,
        grace_until: None,
    };
    let mut ping = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping_id: u32 = 0;
    let result = loop {
        if out.closing() {
            break drain(&mut out, &mut control, &shared).await.err();
        }
        let has_audio = shared.has_audio();
        let frame = tokio::select! {
            biased;
            _ = out.ending.changed() => continue,
            outgoing = control.recv() => match outgoing {
                Some(Outgoing::Control(frame)) => frame,
                // Whoever sent the close recorded the end first.
                Some(Outgoing::Close) => {
                    out.shutdown().await;
                    break None;
                }
                None => {
                    out.shutdown().await;
                    break Some(LinkEnd::Shutdown);
                }
            },
            _ = ping.tick() => {
                ping_id = ping_id.wrapping_add(1);
                match encode(&BridgeMessage::Ping(Ping { id: ping_id })) {
                    Ok(frame) => frame,
                    Err(_) => continue,
                }
            }
            () = std::future::ready(()), if has_audio => match shared.pop_audio() {
                Some(audio) => {
                    bump(&shared.stats.audio_out);
                    Frame::Audio(audio)
                }
                None => continue,
            },
            () = shared.audio_ready.notified(), if !has_audio => continue,
        };
        if let Err(end) = out.write(&frame).await {
            break Some(end);
        }
        bump(&shared.stats.frames_out);
    };
    if let Some(end) = result {
        shared.finish(end);
    }
}

/// Writes the control messages queued before the close, then shuts the writer down.
async fn drain<W: AsyncWrite + Unpin>(
    out: &mut Out<W>,
    control: &mut mpsc::UnboundedReceiver<Outgoing>,
    shared: &Shared,
) -> Result<(), LinkEnd> {
    while let Ok(Outgoing::Control(frame)) = control.try_recv() {
        out.write(&frame).await?;
        bump(&shared.stats.frames_out);
    }
    out.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;

    use push_voice_adapter_proto::{CallEndReason, CallEnded, CallStarted, StopSpeaking};
    use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
    use tokio::time::sleep;

    use super::*;

    const HELLO: &str = r#"{"type":"hello","protocol":"push-voice-adapter/1","name":"fake","version":"1.0","capabilities":{"stt":true,"tts":true},"voices":[{"id":"v1"}]}"#;

    struct Fake {
        reader: AsyncFrameReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Fake {
        async fn send(&mut self, json: &str) {
            self.send_frame(Frame::Control(json.as_bytes().to_vec()))
                .await;
        }

        async fn send_frame(&mut self, frame: Frame) {
            let bytes = frame.to_bytes().unwrap();
            self.writer.write_all(&bytes).await.unwrap();
        }

        async fn next(&mut self) -> Option<Frame> {
            self.reader.read_frame().await.unwrap()
        }

        async fn message(&mut self) -> BridgeMessage {
            match self.next().await {
                Some(Frame::Control(json)) => match decode::<BridgeMessage>(&json).unwrap() {
                    Decoded::Known(message) => message,
                    Decoded::Unknown(name) => panic!("unknown {name}"),
                },
                other => panic!("expected a control message, got {other:?}"),
            }
        }

        async fn audio(&mut self) -> AudioFrame {
            match self.next().await {
                Some(Frame::Audio(audio)) => audio,
                other => panic!("expected audio, got {other:?}"),
            }
        }

        async fn error(&mut self) -> ErrorMessage {
            match self.message().await {
                BridgeMessage::Error(error) => error,
                other => panic!("expected an error, got {other:?}"),
            }
        }

        /// Sends a ping and expects its pong next: everything before it was handled.
        async fn sync(&mut self, id: u32) {
            self.send(&format!(r#"{{"type":"ping","id":{id}}}"#)).await;
            assert_eq!(self.message().await, BridgeMessage::Pong(Pong { id }));
        }
    }

    fn pipe(capacity: usize) -> (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>, Fake) {
        let (bridge, adapter) = tokio::io::duplex(capacity);
        let (read, write) = tokio::io::split(bridge);
        let (adapter_read, adapter_write) = tokio::io::split(adapter);
        let fake = Fake {
            reader: AsyncFrameReader::new(adapter_read),
            writer: adapter_write,
        };
        (read, write, fake)
    }

    fn hello() -> BridgeHello {
        BridgeHello::v1("1.2.3", "en-US")
    }

    async fn open_with(
        capacity: usize,
        answer: &'static str,
    ) -> (
        Result<(Link, mpsc::UnboundedReceiver<LinkEvent>), LinkEnd>,
        Fake,
    ) {
        let (read, write, mut fake) = pipe(capacity);
        tokio::join!(Link::open(read, write, hello()), async move {
            assert!(matches!(fake.message().await, BridgeMessage::Hello(_)));
            fake.send(answer).await;
            fake
        })
    }

    async fn ready_with(capacity: usize) -> (Link, mpsc::UnboundedReceiver<LinkEvent>, Fake) {
        let (opened, fake) = open_with(capacity, HELLO).await;
        let (link, events) = opened.unwrap();
        (link, events, fake)
    }

    async fn ready() -> (Link, mpsc::UnboundedReceiver<LinkEvent>, Fake) {
        ready_with(64 * 1024).await
    }

    fn call_started(call_id: &str) -> BridgeMessage {
        BridgeMessage::CallStarted(CallStarted {
            call_id: call_id.into(),
            peer: "Owner".into(),
            language: "en-US".into(),
        })
    }

    fn call_ended(call_id: &str) -> BridgeMessage {
        BridgeMessage::CallEnded(CallEnded {
            call_id: call_id.into(),
            reason: CallEndReason::Completed,
        })
    }

    fn speech(stream: Stream, samples: usize) -> Frame {
        Frame::Audio(AudioFrame {
            stream,
            seq: 0,
            timestamp_ms: 0,
            samples: vec![1; samples],
        })
    }

    async fn closed(events: &mut mpsc::UnboundedReceiver<LinkEvent>) -> LinkEnd {
        loop {
            match events.recv().await.expect("a Closed event") {
                LinkEvent::Closed(end) => return end,
                _ => continue,
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_hello_exchange_gives_the_adapters_info() {
        let (read, write, mut fake) = pipe(64 * 1024);
        let (opened, sent) = tokio::join!(Link::open(read, write, hello()), async {
            let sent = fake.message().await;
            fake.send(HELLO).await;
            sent
        });
        let BridgeMessage::Hello(sent) = sent else {
            panic!("the bridge's hello first");
        };
        assert_eq!(sent, hello());
        let (link, _events) = opened.unwrap();
        assert_eq!(
            link.info(),
            &AdapterInfo {
                protocol: "push-voice-adapter/1".into(),
                name: "fake".into(),
                version: "1.0".into(),
                capabilities: Capabilities {
                    stt: true,
                    tts: true,
                    ..Capabilities::default()
                },
                voices: vec![Voice {
                    id: "v1".into(),
                    language: None,
                    name: None,
                }],
            }
        );
        assert_eq!(link.end(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn no_hello_within_10_s_fails() {
        let (read, write, mut fake) = pipe(64 * 1024);
        let start = Instant::now();
        let (opened, _fake) = tokio::join!(Link::open(read, write, hello()), async {
            fake.message().await;
            fake
        });
        assert_eq!(
            opened.unwrap_err(),
            LinkEnd::Protocol("the voice adapter sent no hello within 10 s".into())
        );
        assert_eq!(start.elapsed(), HELLO_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_hello_write_counts_against_the_same_deadline() {
        // The pipe holds less than the hello, so writing it waits for the adapter to read.
        let (read, write, mut fake) = pipe(16);
        let start = Instant::now();
        let (opened, _fake) = tokio::join!(Link::open(read, write, hello()), async {
            sleep(Duration::from_secs(6)).await;
            fake.message().await;
            fake
        });
        assert_eq!(
            opened.unwrap_err(),
            LinkEnd::Protocol("the voice adapter sent no hello within 10 s".into())
        );
        assert_eq!(start.elapsed(), HELLO_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_version_not_offered_is_refused() {
        let (opened, mut fake) = open_with(
            64 * 1024,
            r#"{"type":"hello","protocol":"push-voice-adapter/2","name":"x","version":"2","capabilities":{}}"#,
        )
        .await;
        assert_eq!(
            opened.unwrap_err(),
            LinkEnd::Protocol(
                "voice adapter speaks push-voice-adapter/2; this bridge speaks push-voice-adapter/1"
                    .into()
            )
        );
        assert_eq!(fake.error().await.code, ErrorCode::UnsupportedProtocol);
        assert_eq!(fake.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn an_error_instead_of_a_hello_names_the_refusal() {
        let (opened, mut fake) = open_with(
            64 * 1024,
            r#"{"type":"error","code":"unsupported_format","message":"no"}"#,
        )
        .await;
        let LinkEnd::Protocol(reason) = opened.unwrap_err() else {
            panic!("a protocol error");
        };
        assert!(reason.contains("unsupported_format"), "{reason}");
        assert!(reason.contains("16000 Hz"), "{reason}");
        assert_eq!(fake.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_malformed_hello_is_fatal() {
        let (opened, mut fake) = open_with(
            64 * 1024,
            r#"{"type":"hello","protocol":"push-voice-adapter/1"}"#,
        )
        .await;
        assert!(matches!(opened.unwrap_err(), LinkEnd::Protocol(_)));
        assert_eq!(fake.error().await.code, ErrorCode::InvalidMessage);
        assert_eq!(fake.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn pings_are_answered_with_the_same_id() {
        let (_link, _events, mut fake) = ready().await;
        fake.send(r#"{"type":"ping","id":4000000000}"#).await;
        assert_eq!(
            fake.message().await,
            BridgeMessage::Pong(Pong { id: 4_000_000_000 })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_bridge_pings_every_10_s() {
        let (_link, _events, mut fake) = ready().await;
        let start = Instant::now();
        for expected in 1..=3u32 {
            let BridgeMessage::Ping(Ping { id }) = fake.message().await else {
                panic!("a ping");
            };
            assert_eq!(start.elapsed(), PING_INTERVAL * expected);
            fake.send(&format!(r#"{{"type":"pong","id":{id}}}"#)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn silence_for_30_s_is_a_dead_peer() {
        let (link, mut events, _fake) = ready().await;
        let mut signal = link.closed();
        let start = Instant::now();
        assert_eq!(closed(&mut events).await, LinkEnd::DeadPeer);
        assert_eq!(start.elapsed(), DEAD_PEER_AFTER);
        assert_eq!(signal.wait().await, LinkEnd::DeadPeer);
        assert!(events.recv().await.is_none(), "Closed is the last event");
    }

    #[tokio::test(start_paused = true)]
    async fn more_than_100_messages_in_a_second_overload() {
        let (_link, mut events, mut fake) = ready().await;
        for _ in 0..=MAX_CONTROL_PER_SECOND {
            fake.send(r#"{"type":"speech_started","call_id":"c1","utterance_id":"u1"}"#)
                .await;
        }
        assert_eq!(fake.error().await.code, ErrorCode::Overloaded);
        assert_eq!(fake.next().await, None);
        let mut delivered = 0;
        loop {
            match events.recv().await.unwrap() {
                LinkEvent::Message(_) => delivered += 1,
                LinkEvent::Closed(end) => {
                    assert_eq!(end, LinkEnd::Overloaded);
                    break;
                }
                LinkEvent::Audio(_) => panic!("no audio"),
            }
        }
        assert_eq!(delivered, MAX_CONTROL_PER_SECOND);
    }

    #[tokio::test(start_paused = true)]
    async fn partials_over_10_a_second_are_dropped_without_overloading() {
        let (link, mut events, mut fake) = ready().await;
        for _ in 0..30 {
            fake.send(r#"{"type":"transcript","call_id":"c1","utterance_id":"u1","text":"hm","final":false}"#)
                .await;
        }
        // 90 more messages: with the 10 partials kept, exactly the cap.
        for _ in 0..89 {
            fake.send(r#"{"type":"speech_started","call_id":"c1","utterance_id":"u1"}"#)
                .await;
        }
        fake.sync(1).await;
        assert_eq!(link.end(), None);
        let mut partials = 0;
        while let Ok(event) = events.try_recv() {
            if let LinkEvent::Message(AdapterMessage::Transcript(_)) = event {
                partials += 1;
            }
        }
        assert_eq!(partials, MAX_PARTIALS_PER_SECOND);
        assert_eq!(link.stats().partials_dropped, 20);
    }

    #[tokio::test(start_paused = true)]
    async fn an_oversize_header_is_a_framing_error() {
        let (_link, mut events, mut fake) = ready().await;
        // A header announcing a payload over the limit.
        fake.writer.write_all(&[0x7F, 0, 0, 0, 1]).await.unwrap();
        assert_eq!(fake.error().await.code, ErrorCode::InvalidFrame);
        assert_eq!(fake.next().await, None);
        assert!(matches!(closed(&mut events).await, LinkEnd::Framing(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_types_are_ignored() {
        let (link, mut events, mut fake) = ready().await;
        fake.send(r#"{"type":"future_thing","x":1}"#).await;
        fake.sync(1).await;
        assert!(events.try_recv().is_err());
        assert_eq!(link.end(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_malformed_message_is_answered_and_dropped() {
        let (link, mut events, mut fake) = ready().await;
        fake.send(r#"{"type":"speak_done","call_id":"c1"}"#).await;
        assert_eq!(fake.error().await.code, ErrorCode::InvalidMessage);
        fake.sync(1).await;
        assert!(events.try_recv().is_err());
        assert_eq!(link.stats().invalid_messages, 1);
        assert_eq!(link.end(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn bad_audio_is_dropped_with_one_error_per_call() {
        let (link, mut events, mut fake) = ready().await;
        link.send(call_started("c1")).unwrap();
        assert!(matches!(
            fake.message().await,
            BridgeMessage::CallStarted(_)
        ));
        fake.send_frame(speech(Stream::FromCaller, 320)).await;
        let error = fake.error().await;
        assert_eq!(error.code, ErrorCode::UnsupportedFormat);
        assert_eq!(error.call_id.as_deref(), Some("c1"));
        fake.send_frame(speech(Stream::ToCaller, 319)).await;
        fake.sync(1).await;
        fake.send_frame(speech(Stream::ToCaller, 320)).await;
        fake.sync(2).await;
        assert!(matches!(events.try_recv(), Ok(LinkEvent::Audio(_))));
        assert!(events.try_recv().is_err());

        link.send(call_ended("c1")).unwrap();
        link.send(call_started("c2")).unwrap();
        fake.message().await;
        fake.message().await;
        fake.send_frame(speech(Stream::ToCaller, 319)).await;
        assert_eq!(fake.error().await.call_id.as_deref(), Some("c2"));
        let stats = link.stats();
        assert_eq!((stats.audio_in, stats.dropped_from_adapter), (1, 3));
    }

    #[tokio::test(start_paused = true)]
    async fn speech_faster_than_4x_real_time_is_dropped() {
        let (link, mut events, mut fake) = ready().await;
        link.send(call_started("c1")).unwrap();
        fake.message().await;
        for _ in 0..450 {
            fake.send_frame(speech(Stream::ToCaller, 320)).await;
        }
        fake.sync(1).await;
        let mut audio = 0;
        while let Ok(LinkEvent::Audio(_)) = events.try_recv() {
            audio += 1;
        }
        assert_eq!(audio, 400);
        assert_eq!(link.stats().dropped_from_adapter, 50);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_audio_queue_drops_the_oldest_and_control_goes_first() {
        let (link, _events, mut fake) = ready_with(64).await;
        link.send(call_started("c1")).unwrap();
        assert!(matches!(
            fake.message().await,
            BridgeMessage::CallStarted(_)
        ));
        // The fake stops reading: frame 0 sticks in the pipe, the rest queue.
        link.send_audio(&[0; FRAME]);
        sleep(Duration::from_millis(1)).await;
        for _ in 1..=TO_ADAPTER_QUEUE_FRAMES + 1 {
            link.send_audio(&[0; FRAME]);
        }
        assert_eq!(link.stats().dropped_to_adapter, 1);
        link.send(BridgeMessage::StopSpeaking(StopSpeaking {
            call_id: "c1".into(),
            speak_id: None,
        }))
        .unwrap();
        sleep(Duration::from_millis(1)).await;

        assert_eq!(fake.audio().await.seq, 0);
        assert!(matches!(
            fake.message().await,
            BridgeMessage::StopSpeaking(_)
        ));
        for seq in 2..=TO_ADAPTER_QUEUE_FRAMES as u32 + 1 {
            let audio = fake.audio().await;
            assert_eq!((audio.seq, audio.timestamp_ms), (seq, seq * 20));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn seq_and_timestamps_restart_with_each_call() {
        let (link, _events, mut fake) = ready().await;
        link.send_audio(&[0; FRAME]); // no call: not sent
        link.send(call_started("c1")).unwrap();
        for _ in 0..3 {
            link.send_audio(&[5; FRAME]);
        }
        assert!(matches!(
            fake.message().await,
            BridgeMessage::CallStarted(_)
        ));
        for seq in 0..3 {
            let audio = fake.audio().await;
            assert_eq!(audio.stream, Stream::FromCaller);
            assert_eq!((audio.seq, audio.timestamp_ms), (seq, seq * 20));
            assert_eq!(audio.samples, vec![5; FRAME]);
        }
        link.send(call_ended("c1")).unwrap();
        link.send_audio(&[0; FRAME]); // no call: not sent
        link.send(call_started("c2")).unwrap();
        link.send_audio(&[0; FRAME]);
        assert!(matches!(fake.message().await, BridgeMessage::CallEnded(_)));
        assert!(matches!(
            fake.message().await,
            BridgeMessage::CallStarted(_)
        ));
        let audio = fake.audio().await;
        assert_eq!((audio.seq, audio.timestamp_ms), (0, 0));
        assert_eq!(link.stats().audio_out, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn close_flushes_control_then_ends_once() {
        let (link, mut events, mut fake) = ready().await;
        let mut signal = link.closed();
        link.send(call_started("c1")).unwrap();
        link.send(call_ended("c1")).unwrap();
        link.close();
        assert!(matches!(
            fake.message().await,
            BridgeMessage::CallStarted(_)
        ));
        assert!(matches!(fake.message().await, BridgeMessage::CallEnded(_)));
        assert_eq!(fake.next().await, None);
        assert_eq!(closed(&mut events).await, LinkEnd::Shutdown);
        assert!(events.recv().await.is_none());
        assert_eq!(signal.wait().await, LinkEnd::Shutdown);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_every_handle_closes_the_link() {
        let (link, mut events, mut fake) = ready().await;
        let mut signal = link.closed();
        let other = link.clone();
        drop(link);
        assert_eq!(other.end(), None);
        drop(other);
        assert_eq!(fake.next().await, None);
        assert_eq!(closed(&mut events).await, LinkEnd::Shutdown);
        assert_eq!(signal.wait().await, LinkEnd::Shutdown);
    }

    #[tokio::test(start_paused = true)]
    async fn the_adapter_closing_is_eof() {
        let (_link, mut events, fake) = ready().await;
        drop(fake);
        assert_eq!(closed(&mut events).await, LinkEnd::Eof);
    }

    /// The reader records why the link ended before the writer can record `Shutdown`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_end_reason_is_never_misreported_as_shutdown() {
        for _ in 0..200 {
            let (link, mut events, fake) = ready().await;
            let mut signal = link.closed();
            drop(fake);
            assert_eq!(closed(&mut events).await, LinkEnd::Eof);
            assert_eq!(signal.wait().await, LinkEnd::Eof);
            assert_eq!(link.end(), Some(LinkEnd::Eof));
        }
        for _ in 0..200 {
            let (link, mut events, mut fake) = ready().await;
            fake.writer.write_all(&[0x7F, 0, 0, 0, 1]).await.unwrap();
            assert!(matches!(closed(&mut events).await, LinkEnd::Framing(_)));
            assert!(matches!(link.end(), Some(LinkEnd::Framing(_))));
        }
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn logs_carry_no_text() {
        let captured = Captured::default();
        let make = {
            let captured = captured.clone();
            move || captured.clone()
        };
        let _guard = tracing::subscriber::set_default(crate::logging::subscriber(
            make,
            tracing_subscriber::EnvFilter::new("trace"),
        ));
        let (link, mut events, mut fake) = ready().await;
        // A callsite another test thread was registering while this subscriber was set may
        // have cached `never` (tracing's registration race): rebuild once that is over.
        std::thread::sleep(Duration::from_millis(50));
        tracing::callsite::rebuild_interest_cache();
        link.send(BridgeMessage::CallStarted(CallStarted {
            call_id: "c1".into(),
            peer: "SECRET peer".into(),
            language: "en-US".into(),
        }))
        .unwrap();
        fake.message().await;
        fake.send(r#"{"type":"transcript","call_id":"c1","utterance_id":"u1","text":"SECRET words","final":true}"#).await;
        fake.send(r#"{"type":"transcript","call_id":"c1","utterance_id":"u1","text":"SECRET bad","final":true,"confidence":7}"#).await;
        assert_eq!(fake.error().await.code, ErrorCode::InvalidMessage);
        fake.send(r#"{"type":"future_thing","text":"SECRET unknown"}"#)
            .await;
        fake.send(r#"{"type":"error","code":"internal","message":"SECRET failure"}"#)
            .await;
        fake.sync(1).await;
        link.close();
        closed(&mut events).await;
        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("adapter_unknown_type"), "{logs}");
        assert!(logs.contains("adapter_invalid_message"), "{logs}");
        assert!(logs.contains("adapter_error"), "{logs}");
        assert!(!logs.contains("SECRET"), "{logs}");
    }
}
