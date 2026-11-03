//! The task that runs a call's media: str0m's `Rtc` fed from a UDP socket (host and
//! server-reflexive candidates) and a TURN allocation (the relay candidate), Opus in and
//! out, one frame sent every 20 ms, and the call's signalling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use push_domain::CallId;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use super::codec::{Decoder, Encoder, FRAME};
use super::ice::{self, Endpoint, TurnServer};
use super::turn::Relay;
use super::{
    Command, EndCause, FRAME_DURATION, MediaEnded, MediaOptions, MediaState, PcmFrame, Role,
    Signalling,
};
use crate::SdkError;
use crate::calls::{CallEndReason, CallErrorCode, CallEvent, IceCandidate, IceServers, SdpKind};

/// RTP ticks of one 20 ms Opus packet (48 kHz clock).
const RTP_FRAME: u64 = 960;
/// Remote candidates held until the remote description is set.
const MAX_HELD: usize = 64;

/// The driver's ends of the channels to its [`super::MediaCall`].
pub(crate) struct Channels {
    pub(crate) commands: mpsc::UnboundedReceiver<Command>,
    pub(crate) state: watch::Sender<MediaState>,
    pub(crate) incoming: mpsc::Sender<PcmFrame>,
    pub(crate) outgoing: mpsc::Receiver<(u64, PcmFrame)>,
    pub(crate) generation: Arc<AtomicU64>,
}

/// Gathers, negotiates (invite or accept) and spawns the driver; the call's id.
pub(crate) async fn start<S: Signalling>(
    signalling: S,
    servers: &IceServers,
    role: Role,
    options: &MediaOptions,
    channels: Channels,
) -> Result<CallId, SdkError> {
    let relay_only = options.relay_only.unwrap_or(servers.relay);
    let (stun, turn) = ice::usable(servers);
    let gathered = gather(relay_only, &stun, &turn, options.gather_timeout).await?;
    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    if let Some((host, _)) = &gathered.host {
        add_local(&mut rtc, Candidate::host(*host, "udp"));
        if let Some(mapped) = gathered.mapped {
            add_local(&mut rtc, Candidate::server_reflexive(mapped, *host, "udp"));
        }
    }
    if let Some(relay) = &gathered.relay {
        add_local(
            &mut rtc,
            Candidate::relayed(relay.relayed, relay.local(), "udp"),
        );
    }
    let polite = matches!(role, Role::Callee { .. });
    let mut driver = Driver {
        signalling,
        call_id: CallId::new(),
        polite,
        options: options.clone(),
        rtc,
        host: gathered.host,
        relay: gathered.relay,
        mid: None,
        pt: None,
        pending: None,
        remote: false,
        held: Vec::new(),
        encoder: Encoder::new().map_err(SdkError::Network)?,
        decoder: Decoder::new().map_err(SdkError::Network)?,
        partial: Vec::new(),
        channels,
        rtp_time: u64::from(rand::random::<u32>()),
        connected: false,
        timeout: Instant::now(),
        connect_deadline: None,
        broken_since: None,
        ended: None,
    };
    match role {
        Role::Caller { peer } => {
            let mut api = driver.rtc.sdp_api();
            driver.mid =
                Some(api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None));
            let (offer, pending) = api
                .apply()
                .ok_or_else(|| SdkError::Network("no offer to make".to_owned()))?;
            driver.pending = Some(pending);
            driver.drain().await.map_err(SdkError::Network)?;
            driver.call_id = driver
                .signalling
                .invite(peer, offer.to_sdp_string())
                .await?;
        }
        Role::Callee { call_id, offer_sdp } => {
            driver.call_id = call_id;
            let offer = SdpOffer::from_sdp_string(&offer_sdp)
                .map_err(|error| SdkError::Invalid(format!("the offer: {error}")))?;
            let answer = driver
                .rtc
                .sdp_api()
                .accept_offer(offer)
                .map_err(|error| SdkError::Invalid(format!("the offer: {error}")))?;
            driver.drain().await.map_err(SdkError::Network)?;
            let answer_sdp = answer.to_sdp_string();
            // str0m reports the offered media (Event::MediaAdded) only later.
            if driver.mid.is_none() {
                driver.mid = audio_mid(&answer_sdp);
            }
            if driver.payload_type().is_none() {
                return Err(SdkError::Invalid("the offer has no Opus audio".to_owned()));
            }
            driver.signalling.accept(call_id, answer_sdp).await?;
            driver.remote_set();
        }
    }
    // Every candidate is in the description: the trickle ends at once.
    let _ = driver
        .signalling
        .send_ice(driver.call_id, end_of_candidates(driver.mid))
        .await;
    let call_id = driver.call_id;
    tokio::spawn(driver.run());
    Ok(call_id)
}

fn add_local(rtc: &mut Rtc, candidate: Result<Candidate, str0m::error::IceError>) {
    match candidate {
        Ok(candidate) => {
            rtc.add_local_candidate(candidate);
        }
        Err(error) => tracing::debug!(%error, "calls media: a local candidate is unusable"),
    }
}

/// The =mid of the first m=audio section of sdp.
fn audio_mid(sdp: &str) -> Option<Mid> {
    let mut audio = false;
    for line in sdp.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            audio = media.starts_with("audio ");
        } else if let Some(mid) = line.strip_prefix("a=mid:").filter(|_| audio) {
            return Some(Mid::from(mid.trim()));
        }
    }
    None
}

fn end_of_candidates(mid: Option<Mid>) -> IceCandidate {
    IceCandidate {
        sdp_mid: mid.map(|mid| mid.to_string()),
        sdp_mline_index: Some(0),
        ..IceCandidate::end_of_candidates()
    }
}

/// What gathering found.
struct Gathered {
    /// The UDP socket of the host (and server-reflexive) candidate with its address.
    host: Option<(SocketAddr, UdpSocket)>,
    /// The host candidate's address as a STUN server sees it, when it differs.
    mapped: Option<SocketAddr>,
    relay: Option<Relay>,
}

async fn gather(
    relay_only: bool,
    stun: &[Endpoint],
    turn: &[TurnServer],
    timeout: Duration,
) -> Result<Gathered, SdkError> {
    let host = if relay_only {
        None
    } else {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|error| SdkError::Network(error.to_string()))?;
        let port = socket
            .local_addr()
            .map_err(|error| SdkError::Network(error.to_string()))?
            .port();
        Some((SocketAddr::new(ice::local_ip(), port), socket))
    };
    let reflexive = async {
        match &host {
            Some((address, socket)) if !stun.is_empty() => {
                tokio::time::timeout(timeout, mapped_address(socket, stun))
                    .await
                    .ok()
                    .flatten()
                    .filter(|mapped| mapped != address)
            }
            _ => None,
        }
    };
    let relayed = async {
        for server in turn {
            match tokio::time::timeout(timeout, Relay::allocate(server)).await {
                Ok(Ok(relay)) => return Some(relay),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "calls media: no TURN allocation on a server");
                }
                Err(_) => tracing::debug!("calls media: a TURN allocation timed out"),
            }
        }
        None
    };
    let (mapped, relay) = tokio::join!(reflexive, relayed);
    if relay_only && relay.is_none() {
        return Err(SdkError::Network(
            "relay-only ICE without a TURN allocation".to_owned(),
        ));
    }
    Ok(Gathered {
        host,
        mapped,
        relay,
    })
}

/// The address the first answering STUN server sees `socket` at.
async fn mapped_address(socket: &UdpSocket, stun: &[Endpoint]) -> Option<SocketAddr> {
    let mut servers = Vec::new();
    for endpoint in stun {
        if let Ok(addresses) =
            tokio::net::lookup_host((endpoint.host.as_str(), endpoint.port)).await
        {
            servers.extend(addresses.filter(SocketAddr::is_ipv4));
        }
    }
    if servers.is_empty() {
        return None;
    }
    let transaction: [u8; 12] = rand::random();
    let request = ice::binding_request(transaction);
    let mut buffer = [0u8; 512];
    loop {
        for server in &servers {
            let _ = socket.send_to(&request, server).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while let Ok(Ok((length, source))) =
            tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
        {
            if servers.contains(&source) {
                if let Some(mapped) = ice::binding_response(&buffer[..length], transaction) {
                    return Some(mapped);
                }
            }
        }
    }
}

struct Driver<S> {
    signalling: S,
    call_id: CallId,
    /// The callee: it yields when both sides offer at once.
    polite: bool,
    options: MediaOptions,
    rtc: Rtc,
    host: Option<(SocketAddr, UdpSocket)>,
    relay: Option<Relay>,
    mid: Option<Mid>,
    pt: Option<Pt>,
    /// This side's offer waiting for its answer.
    pending: Option<SdpPendingOffer>,
    /// Remote candidates are applied once the remote description is set.
    remote: bool,
    held: Vec<IceCandidate>,
    encoder: Encoder,
    decoder: Decoder,
    /// Decoded samples short of a whole frame.
    partial: Vec<i16>,
    channels: Channels,
    rtp_time: u64,
    connected: bool,
    /// When `rtc` next wants time.
    timeout: Instant,
    connect_deadline: Option<Instant>,
    broken_since: Option<Instant>,
    ended: Option<MediaEnded>,
}

impl<S: Signalling> Driver<S> {
    async fn run(mut self) {
        let mut tick = tokio::time::interval(FRAME_DURATION);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut host_buffer = vec![0u8; 2_048];
        let mut relay_buffer = vec![0u8; 4_096];
        while self.ended.is_none() {
            let deadline = tokio::time::Instant::from_std(self.deadline());
            tokio::select! {
                command = self.channels.commands.recv() => match command {
                    Some(Command::Event(event)) => self.on_call_event(event).await,
                    Some(Command::Hangup) | None => self.hang_up().await,
                },
                received = recv_host(self.host.as_ref(), &mut host_buffer) => {
                    if let Some((length, source)) = received {
                        self.on_host(source, &host_buffer[..length]);
                    }
                }
                read = recv_relay(self.relay.as_mut(), &mut relay_buffer) => match read {
                    Ok(length) => self.on_relay(&relay_buffer[..length]).await,
                    Err(error) => {
                        tracing::debug!(%error, "calls media: the TURN link closed");
                        self.relay = None;
                    }
                },
                _ = tick.tick() => self.send_audio(),
                () = tokio::time::sleep_until(deadline) => self.on_timeout().await,
            }
            if self.ended.is_none() {
                if let Err(error) = self.drain().await {
                    tracing::debug!(%error, "calls media: the WebRTC session failed");
                    self.fail().await;
                }
            }
        }
        if let Some(relay) = &mut self.relay {
            relay.release().await;
        }
        self.rtc.disconnect();
    }

    /// The soonest of every timer.
    fn deadline(&mut self) -> Instant {
        let mut deadline = self.timeout;
        if let Some(at) = self.relay.as_mut().and_then(Relay::deadline) {
            deadline = deadline.min(at);
        }
        if let Some(at) = self.connect_deadline {
            deadline = deadline.min(at);
        }
        if let Some(since) = self.broken_since {
            deadline = deadline.min(since + self.options.disconnect_timeout);
        }
        deadline
    }

    /// Runs `rtc` until it waits for time: what it sends goes out, what it reports is
    /// handled. Due after every change to `rtc`.
    async fn drain(&mut self) -> Result<(), String> {
        loop {
            match self.rtc.poll_output().map_err(|error| error.to_string())? {
                Output::Timeout(at) => {
                    self.timeout = at;
                    return Ok(());
                }
                Output::Transmit(transmit) => {
                    self.transmit(transmit.source, transmit.destination, &transmit.contents)
                        .await;
                }
                Output::Event(event) => self.on_rtc_event(event),
            }
        }
    }

    async fn transmit(&mut self, source: SocketAddr, destination: SocketAddr, data: &[u8]) {
        if let Some(relay) = self.relay.as_mut().filter(|relay| relay.relayed == source) {
            if let Err(error) = relay.send(destination, data).await {
                tracing::debug!(%error, "calls media: sending through TURN failed");
            }
        } else if let Some((_, socket)) = &self.host {
            let _ = socket.send_to(data, destination).await;
        }
    }

    fn on_rtc_event(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.connected = true;
                self.connect_deadline = None;
                self.set_state(MediaState::Connected);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.broken_since.get_or_insert_with(Instant::now);
            }
            Event::IceConnectionStateChange(
                IceConnectionState::Connected | IceConnectionState::Completed,
            ) => self.broken_since = None,
            Event::MediaAdded(added) if added.kind == MediaKind::Audio => {
                self.mid.get_or_insert(added.mid);
            }
            Event::MediaData(data) => match self.decoder.decode(&data.data) {
                Ok(samples) => {
                    self.partial.extend(samples);
                    while self.partial.len() >= FRAME {
                        let rest = self.partial.split_off(FRAME);
                        let frame = std::mem::replace(&mut self.partial, rest);
                        if let Some(frame) = PcmFrame::from_slice(&frame) {
                            // A reader too far behind loses this frame.
                            let _ = self.channels.incoming.try_send(frame);
                        }
                    }
                }
                Err(error) => tracing::debug!(%error, "calls media: an undecodable packet"),
            },
            _ => {}
        }
    }

    /// The negotiated Opus payload type.
    fn payload_type(&mut self) -> Option<Pt> {
        if self.pt.is_none() {
            let mid = self.mid?;
            let writer = self.rtc.writer(mid)?;
            self.pt = writer
                .payload_params()
                .find(|params| params.spec().codec == Codec::Opus)
                .map(|params| params.pt());
        }
        self.pt
    }

    /// Every 20 ms once connected: the next queued frame, if any, goes out.
    fn send_audio(&mut self) {
        if !self.connected {
            return;
        }
        let generation = self.channels.generation.load(Ordering::Acquire);
        let frame = loop {
            match self.channels.outgoing.try_recv() {
                Ok((queued, frame)) if queued == generation => break Some(frame),
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        let rtp_time = self.rtp_time;
        self.rtp_time += RTP_FRAME;
        let Some(frame) = frame else {
            return;
        };
        let (Some(mid), Some(pt)) = (self.mid, self.payload_type()) else {
            return;
        };
        let packet = match self.encoder.encode(frame.samples()) {
            Ok(packet) => packet,
            Err(error) => {
                tracing::debug!(%error, "calls media: Opus refused a frame");
                return;
            }
        };
        if let Some(writer) = self.rtc.writer(mid) {
            let time = MediaTime::new(rtp_time, Frequency::FORTY_EIGHT_KHZ);
            if let Err(error) = writer.write(pt, Instant::now(), time, packet) {
                tracing::debug!(%error, "calls media: a frame was not sent");
            }
        }
    }

    fn on_host(&mut self, source: SocketAddr, packet: &[u8]) {
        let Some((destination, _)) = &self.host else {
            return;
        };
        if let Ok(receive) = Receive::new(Protocol::Udp, source, *destination, packet) {
            self.feed(Input::Receive(Instant::now(), receive));
        }
    }

    async fn on_relay(&mut self, bytes: &[u8]) {
        let Some(relay) = self.relay.as_mut() else {
            return;
        };
        let relayed = relay.relayed;
        let received = relay.input(bytes);
        if let Err(error) = relay.flush().await {
            tracing::debug!(%error, "calls media: writing to the TURN server failed");
        }
        for (peer, data) in received {
            if let Ok(receive) = Receive::new(Protocol::Udp, peer, relayed, &data) {
                self.feed(Input::Receive(Instant::now(), receive));
                if self.drain().await.is_err() {
                    return;
                }
            }
        }
    }

    /// One input for `rtc` (drained by the caller); inputs it does not take are dropped.
    fn feed(&mut self, input: Input<'_>) {
        if self.rtc.accepts(&input) {
            if let Err(error) = self.rtc.handle_input(input) {
                tracing::debug!(%error, "calls media: an input was refused");
            }
        }
    }

    async fn on_timeout(&mut self) {
        let now = Instant::now();
        self.feed(Input::Timeout(now));
        if let Some(relay) = self.relay.as_mut() {
            if let Err(error) = relay.flush().await {
                tracing::debug!(%error, "calls media: writing to the TURN server failed");
            }
        }
        let never_connected = self.connect_deadline.is_some_and(|at| at <= now);
        let broken = self
            .broken_since
            .is_some_and(|since| since + self.options.disconnect_timeout <= now);
        if never_connected || broken {
            self.fail().await;
        }
    }

    async fn on_call_event(&mut self, event: CallEvent) {
        match event {
            CallEvent::Accepted { answer_sdp, .. } => {
                self.on_answer(&answer_sdp).await;
            }
            CallEvent::PeerIce { candidate, .. } => {
                if candidate.end_of_candidates || candidate.candidate.is_empty() {
                    return;
                }
                if self.remote {
                    self.add_remote(&candidate).await;
                } else if self.held.len() < MAX_HELD {
                    self.held.push(candidate);
                }
            }
            CallEvent::PeerRenegotiate { sdp, kind, .. } => match kind {
                SdpKind::Offer => self.on_offer(&sdp).await,
                SdpKind::Answer => self.on_answer(&sdp).await,
            },
            CallEvent::Ended {
                reason, duration_s, ..
            } => self.end(reason, duration_s, EndCause::Server),
            CallEvent::Synced { calls, gone } => {
                if gone.contains(&self.call_id) {
                    self.end(CallEndReason::Failed, None, EndCause::Gone);
                } else if calls
                    .iter()
                    .any(|call| call.call_id == self.call_id && call.needs_ice_restart())
                {
                    self.restart_ice().await;
                }
            }
            CallEvent::Error { code, .. } => match code {
                CallErrorCode::AlreadyAnswered
                | CallErrorCode::CallNotFound
                | CallErrorCode::NotContact
                | CallErrorCode::InCall
                | CallErrorCode::RateLimited
                | CallErrorCode::NotAvailable => {
                    self.end(CallEndReason::Failed, None, EndCause::Refused(code));
                }
                _ => tracing::debug!(?code, "calls media: a call frame was refused"),
            },
            CallEvent::Incoming { .. }
            | CallEvent::Taken { .. }
            | CallEvent::HistoryChanged { .. } => {}
        }
    }

    /// The answer to this side's offer (the call's, or a renegotiation's).
    async fn on_answer(&mut self, sdp: &str) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let applied = SdpAnswer::from_sdp_string(sdp)
            .map_err(|error| error.to_string())
            .and_then(|answer| {
                self.rtc
                    .sdp_api()
                    .accept_answer(pending, answer)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = applied {
            tracing::debug!(%error, "calls media: the answer is unusable");
            self.fail().await;
            return;
        }
        if self.drain().await.is_err() {
            return self.fail().await;
        }
        if !self.remote {
            self.remote_set();
            for candidate in std::mem::take(&mut self.held) {
                self.add_remote(&candidate).await;
            }
        }
    }

    /// The peer's new offer (an ICE restart, for one). When both sides offer at once the
    /// callee yields (perfect negotiation: the callee is polite).
    async fn on_offer(&mut self, sdp: &str) {
        if self.pending.is_some() && !self.polite {
            return;
        }
        // Accepting an offer rolls back this side's own.
        self.pending = None;
        let answer = SdpOffer::from_sdp_string(sdp)
            .map_err(|error| error.to_string())
            .and_then(|offer| {
                self.rtc
                    .sdp_api()
                    .accept_offer(offer)
                    .map_err(|error| error.to_string())
            });
        match answer {
            Ok(answer) => {
                if self.drain().await.is_err() {
                    return self.fail().await;
                }
                let _ = self
                    .signalling
                    .renegotiate(self.call_id, answer.to_sdp_string(), SdpKind::Answer)
                    .await;
            }
            Err(error) => tracing::debug!(%error, "calls media: the new offer is unusable"),
        }
    }

    /// After a reconnect (`CallState::needs_ice_restart`): a new offer with new ICE
    /// credentials, as the browser recipe does.
    async fn restart_ice(&mut self) {
        if self.pending.is_some() || !self.remote {
            return;
        }
        let mut api = self.rtc.sdp_api();
        api.ice_restart(true);
        let Some((offer, pending)) = api.apply() else {
            return;
        };
        self.pending = Some(pending);
        if self.drain().await.is_err() {
            return self.fail().await;
        }
        let _ = self
            .signalling
            .renegotiate(self.call_id, offer.to_sdp_string(), SdpKind::Offer)
            .await;
    }

    async fn add_remote(&mut self, candidate: &IceCandidate) {
        let line = candidate
            .candidate
            .strip_prefix("a=")
            .unwrap_or(&candidate.candidate);
        match Candidate::from_sdp_string(line) {
            Ok(candidate) => {
                self.rtc.add_remote_candidate(candidate);
                if self.drain().await.is_err() {
                    self.fail().await;
                }
            }
            // mDNS (.local) names and TCP candidates, among others.
            Err(error) => tracing::debug!(%error, "calls media: a remote candidate is unusable"),
        }
    }

    fn remote_set(&mut self) {
        self.remote = true;
        if !self.connected {
            self.connect_deadline = Some(Instant::now() + self.options.connect_timeout);
        }
    }

    async fn hang_up(&mut self) {
        if self.ended.is_none() {
            let _ = self.signalling.hangup(self.call_id, false).await;
            self.end(CallEndReason::Completed, None, EndCause::Local);
        }
    }

    async fn fail(&mut self) {
        if self.ended.is_none() {
            let _ = self.signalling.hangup(self.call_id, true).await;
            self.end(CallEndReason::Failed, None, EndCause::MediaFailed);
        }
    }

    fn end(&mut self, reason: CallEndReason, duration_s: Option<u32>, cause: EndCause) {
        if self.ended.is_none() {
            let ended = MediaEnded {
                reason,
                duration_s,
                cause,
            };
            self.ended = Some(ended);
            self.set_state(MediaState::Ended(ended));
        }
    }

    fn set_state(&self, state: MediaState) {
        self.channels.state.send_if_modified(|current| {
            let changed = !matches!(current, MediaState::Ended(_)) && *current != state;
            if changed {
                *current = state;
            }
            changed
        });
    }
}

async fn recv_host(
    host: Option<&(SocketAddr, UdpSocket)>,
    buffer: &mut [u8],
) -> Option<(usize, SocketAddr)> {
    match host {
        Some((_, socket)) => socket.recv_from(buffer).await.ok(),
        None => std::future::pending().await,
    }
}

async fn recv_relay(relay: Option<&mut Relay>, buffer: &mut [u8]) -> std::io::Result<usize> {
    match relay {
        Some(relay) => relay.read(buffer).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use push_domain::{DeviceId, UserId};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

    use super::*;
    use crate::calls::{CallEndReason, CallEvent, IceCandidate, SdpKind};
    use crate::media::resample::tests::{amplitude, tone};
    use crate::media::{EndCause, FRAME_SAMPLES, MediaCall, MediaEnded};

    /// One side's signalling, delivered straight to the other side as the server would.
    #[derive(Clone)]
    struct Wire {
        to_peer: UnboundedSender<CallEvent>,
        to_self: UnboundedSender<CallEvent>,
    }

    fn wires() -> (
        (Wire, UnboundedReceiver<CallEvent>),
        (Wire, UnboundedReceiver<CallEvent>),
    ) {
        let (a, a_events) = unbounded_channel();
        let (b, b_events) = unbounded_channel();
        (
            (
                Wire {
                    to_peer: b.clone(),
                    to_self: a.clone(),
                },
                a_events,
            ),
            (
                Wire {
                    to_peer: a,
                    to_self: b,
                },
                b_events,
            ),
        )
    }

    impl Signalling for Wire {
        async fn invite(&self, _peer: UserId, offer_sdp: String) -> Result<CallId, SdkError> {
            let call_id = CallId::new();
            let _ = self.to_peer.send(CallEvent::Incoming {
                call_id,
                caller_id: UserId::new(),
                offer_sdp,
                expires_at_ms: 0,
            });
            Ok(call_id)
        }

        async fn accept(&self, call_id: CallId, answer_sdp: String) -> Result<(), SdkError> {
            let _ = self.to_peer.send(CallEvent::Accepted {
                call_id,
                answer_sdp,
                answered_device_id: DeviceId::new(),
            });
            Ok(())
        }

        async fn send_ice(&self, call_id: CallId, candidate: IceCandidate) -> Result<(), SdkError> {
            let _ = self.to_peer.send(CallEvent::PeerIce { call_id, candidate });
            Ok(())
        }

        async fn renegotiate(
            &self,
            call_id: CallId,
            sdp: String,
            kind: SdpKind,
        ) -> Result<(), SdkError> {
            let _ = self
                .to_peer
                .send(CallEvent::PeerRenegotiate { call_id, sdp, kind });
            Ok(())
        }

        async fn hangup(&self, call_id: CallId, failed: bool) -> Result<(), SdkError> {
            let reason = if failed {
                CallEndReason::Failed
            } else {
                CallEndReason::Completed
            };
            for side in [&self.to_peer, &self.to_self] {
                let _ = side.send(CallEvent::Ended {
                    call_id,
                    reason,
                    duration_s: Some(1),
                });
            }
            Ok(())
        }
    }

    fn direct() -> (IceServers, MediaOptions) {
        let servers = IceServers {
            servers: Vec::new(),
            expires_at_ms: 0,
            relay: false,
        };
        let options = MediaOptions {
            connect_timeout: Duration::from_secs(10),
            ..MediaOptions::default()
        };
        (servers, options)
    }

    /// Forwards a side's events to its call until they stop.
    fn forward(call: Arc<MediaCall>, mut events: UnboundedReceiver<CallEvent>) {
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                call.handle(&event);
            }
        });
    }

    /// 2 s of `frequency` at 16 kHz into `call`'s sink, queued at once (the driver paces).
    async fn speak(call: &MediaCall, frequency: f64) {
        let sink = call.outgoing();
        for index in 0..100 {
            let samples = tone(frequency, 16_000.0, FRAME_SAMPLES, index * FRAME_SAMPLES);
            assert!(sink.send(PcmFrame::from_slice(&samples).unwrap()).await);
        }
    }

    /// 1 s of what arrives, after the first 0.2 s.
    async fn listen(stream: &mut crate::media::PcmStream) -> Vec<i16> {
        let mut heard = Vec::new();
        for index in 0..60 {
            let frame = tokio::time::timeout(Duration::from_secs(5), stream.recv())
                .await
                .expect("audio arrives")
                .expect("the call is running");
            if index >= 10 {
                heard.extend_from_slice(frame.samples());
            }
        }
        heard
    }

    #[tokio::test]
    async fn two_helpers_talk_over_host_candidates_and_hang_up() {
        let ((a_wire, a_events), (b_wire, mut b_events)) = wires();
        let (servers, options) = direct();
        let mut a = MediaCall::start_with(
            a_wire,
            servers.clone(),
            Role::Caller {
                peer: UserId::new(),
            },
            options.clone(),
        )
        .await
        .expect("the caller starts");
        let Some(CallEvent::Incoming {
            call_id, offer_sdp, ..
        }) = b_events.recv().await
        else {
            panic!("the callee rings");
        };
        assert_eq!(call_id, a.call_id());
        let mut b = MediaCall::start_with(
            b_wire,
            servers,
            Role::Callee { call_id, offer_sdp },
            options,
        )
        .await
        .expect("the callee answers");
        let (mut a_heard, mut b_heard) = (a.incoming().unwrap(), b.incoming().unwrap());
        assert!(a.incoming().is_none());
        let (a, b) = (Arc::new(a), Arc::new(b));
        forward(a.clone(), a_events);
        forward(b.clone(), b_events);
        tokio::time::timeout(Duration::from_secs(15), async {
            a.connected().await.expect("the caller connects");
            b.connected().await.expect("the callee connects");
        })
        .await
        .expect("the media connects in time");

        tokio::join!(speak(&a, 440.0), speak(&b, 880.0));
        let (at_b, at_a) = tokio::join!(listen(&mut b_heard), listen(&mut a_heard));
        let (b_440, b_880) = (
            amplitude(&at_b, 440.0, 16_000.0),
            amplitude(&at_b, 880.0, 16_000.0),
        );
        assert!(b_440 > 3_000.0 && b_880 < b_440 / 10.0, "{b_440} {b_880}");
        let (a_880, a_440) = (
            amplitude(&at_a, 880.0, 16_000.0),
            amplitude(&at_a, 440.0, 16_000.0),
        );
        assert!(a_880 > 3_000.0 && a_440 < a_880 / 10.0, "{a_880} {a_440}");

        let ended = a.hangup().await;
        assert_eq!(ended.cause, EndCause::Local);
        assert_eq!(ended.reason, CallEndReason::Completed);
        let ended: MediaEnded = tokio::time::timeout(Duration::from_secs(5), b.on_ended())
            .await
            .expect("the peer's call ends");
        assert_eq!(
            (ended.reason, ended.cause, ended.duration_s),
            (CallEndReason::Completed, EndCause::Server, Some(1))
        );
        assert!(
            b_heard.recv().await.is_none(),
            "the stream ends with the call"
        );
    }

    #[tokio::test]
    async fn relay_only_without_a_relay_is_refused() {
        let ((a_wire, _a_events), _) = wires();
        let (mut servers, options) = direct();
        servers.relay = true;
        let error = MediaCall::start_with(
            a_wire,
            servers,
            Role::Caller {
                peer: UserId::new(),
            },
            options,
        )
        .await
        .expect_err("no relay candidate");
        assert!(matches!(error, SdkError::Network(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_offer_without_audio_is_invalid() {
        let ((a_wire, _a_events), _) = wires();
        let (servers, options) = direct();
        let error = MediaCall::start_with(
            a_wire,
            servers,
            Role::Callee {
                call_id: CallId::new(),
                offer_sdp: "v=0\r\n".to_owned(),
            },
            options,
        )
        .await
        .expect_err("an unusable offer");
        assert!(matches!(error, SdkError::Invalid(_)), "{error:?}");
    }

    #[tokio::test]
    async fn the_media_fails_when_the_peer_never_connects() {
        let ((a_wire, a_events), (b_wire, mut b_events)) = wires();
        let (servers, mut options) = direct();
        options.connect_timeout = Duration::from_secs(1);
        let a = MediaCall::start_with(
            a_wire,
            servers.clone(),
            Role::Caller {
                peer: UserId::new(),
            },
            options.clone(),
        )
        .await
        .unwrap();
        let Some(CallEvent::Incoming { call_id, .. }) = b_events.recv().await else {
            panic!("the callee rings");
        };
        // The callee answers with a description of media that is not there: a second
        // caller's offer turned into an answer by a helper that is dropped at once.
        let (answer_wire, mut answer_events) = {
            let ((wire, _), (_, events)) = wires();
            (wire, events)
        };
        drop(b_wire);
        let ghost = MediaCall::start_with(
            answer_wire,
            servers,
            Role::Caller {
                peer: UserId::new(),
            },
            options,
        )
        .await
        .unwrap();
        let Some(CallEvent::Incoming { offer_sdp, .. }) = answer_events.recv().await else {
            panic!("the ghost's offer");
        };
        drop(ghost);
        let a = Arc::new(a);
        forward(a.clone(), a_events);
        // An offer is no answer, but its candidates and fingerprint never answer either:
        // turn it into one by its setup line.
        let answer_sdp = offer_sdp.replace("a=setup:actpass", "a=setup:active");
        assert!(a.handle(&CallEvent::Accepted {
            call_id,
            answer_sdp,
            answered_device_id: DeviceId::new(),
        }));
        let ended = tokio::time::timeout(Duration::from_secs(10), a.on_ended())
            .await
            .expect("the helper gives up");
        assert_eq!(
            (ended.reason, ended.cause),
            (CallEndReason::Failed, EndCause::MediaFailed)
        );
    }
}
