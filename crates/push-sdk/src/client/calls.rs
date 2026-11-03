//! 1:1 audio calls: the signalling frames (docs/calls.md; spec §10.1 in
//! docs/superpowers/specs/2026-11-03-voice-calls-design.md).

use push_domain::calls::{CandidateInput, check_candidate, check_sdp};
use push_domain::{CallId, UserId};
use push_proto::v1::{self, frame::Body};

use super::Client;
use crate::SdkError;
use crate::calls::{CallPage, IceCandidate, IceServers, SdpKind};
use crate::connection::Command;
use crate::model::ConnectionState;
use crate::store::bytes;

/// The calls of a [`Client`] ([`Client::calls`]): a cheap handle, a clone of the client.
///
/// The SDK carries the signalling; the app runs the media (WebRTC) and gives the SDK its
/// SDP and ICE candidates. Every call frame is fire and forget, like
/// [`Client::send_activity`]: it is checked locally (an SDP of at most 16 KiB, audio
/// only; a well-formed candidate; [`SdkError::Invalid`] otherwise), needs a live
/// connection ([`SdkError::Offline`] otherwise) on which calls are available
/// ([`Calls::available`]; the server's `not_available` otherwise), and returns once
/// queued. The server never answers a call frame directly: what follows arrives as
/// [`Event::Call`], a refusal as [`CallEvent::Error`].
///
/// [`Event::Call`]: crate::Event::Call
/// [`CallEvent::Error`]: crate::CallEvent::Error
#[derive(Clone, Debug)]
pub struct Calls {
    client: Client,
}

impl Client {
    /// The calls of this client (only with [`Config::calls`](super::Config::calls)).
    pub fn calls(&self) -> Calls {
        Calls {
            client: self.clone(),
        }
    }
}

impl Calls {
    /// Whether calls are available now: the client was opened with
    /// [`Config::calls`](super::Config::calls) and the live connection's server offers
    /// them. False while not connected. Hide call UI while it is false.
    pub fn available(&self) -> bool {
        self.client.inner.shared.calls_available()
    }

    /// Calls `peer` (an accepted contact, or the user's agent or owner) with the app's
    /// SDP offer: their calls-capable devices ring. Returns the new call's id. The callee's
    /// answer arrives as [`CallEvent::Accepted`]; a call that cannot ring ends at once
    /// ([`CallEvent::Ended`], busy or unreachable).
    ///
    /// [`CallEvent::Accepted`]: crate::CallEvent::Accepted
    /// [`CallEvent::Ended`]: crate::CallEvent::Ended
    pub async fn invite(&self, peer: UserId, offer_sdp: &str) -> Result<CallId, SdkError> {
        checked_sdp(offer_sdp)?;
        let me = self.client.inner.shared.store().owner().ok().flatten();
        if me == Some(peer) {
            return Err(SdkError::Invalid(
                "a user cannot call themselves".to_owned(),
            ));
        }
        self.usable()?;
        let call = CallId::new();
        // Recorded first: the answer may arrive before this returns.
        self.client.inner.shared.call_invited(call);
        let sent = self.send(Body::CallInvite(v1::CallInvite {
            call_id: bytes(call.0),
            callee_user_id: bytes(peer.0),
            offer_sdp: offer_sdp.to_owned(),
            media: vec![v1::CallMedia::Audio.into()],
        }));
        if let Err(error) = sent {
            self.client.inner.shared.call_unsent(call);
            return Err(error);
        }
        Ok(call)
    }

    /// Answers a ringing call with the app's SDP answer. The first device to answer wins;
    /// a later one gets [`CallErrorCode::AlreadyAnswered`].
    ///
    /// [`CallErrorCode::AlreadyAnswered`]: crate::CallErrorCode::AlreadyAnswered
    pub async fn accept(&self, call: CallId, answer_sdp: &str) -> Result<(), SdkError> {
        checked_sdp(answer_sdp)?;
        self.usable()?;
        // Recorded first: the call may end before this returns.
        let new = self.client.inner.shared.call_live(call);
        let sent = self.send(Body::CallAccept(v1::CallAccept {
            call_id: bytes(call.0),
            answer_sdp: answer_sdp.to_owned(),
        }));
        if sent.is_err() && new {
            self.client.inner.shared.call_unsent(call);
        }
        sent
    }

    /// Refuses a ringing call: declined, or busy when `busy` (the user is in a call
    /// elsewhere). It ends for every device involved.
    pub async fn decline(&self, call: CallId, busy: bool) -> Result<(), SdkError> {
        self.usable()?;
        self.send(Body::CallDecline(v1::CallDecline {
            call_id: bytes(call.0),
            reason: busy.then(|| v1::CallEndReason::Busy.into()),
        }))
    }

    /// Withdraws the user's ringing call. If a callee device answered first, it is
    /// [`CallErrorCode::AlreadyAnswered`]: the call is connected, hang up.
    ///
    /// [`CallErrorCode::AlreadyAnswered`]: crate::CallErrorCode::AlreadyAnswered
    pub async fn cancel(&self, call: CallId) -> Result<(), SdkError> {
        self.usable()?;
        self.send(Body::CallCancel(v1::CallCancel {
            call_id: bytes(call.0),
        }))
    }

    /// Ends a call: completed, or failed when `failed` (the media could not be
    /// established or broke).
    pub async fn hangup(&self, call: CallId, failed: bool) -> Result<(), SdkError> {
        self.usable()?;
        self.send(Body::CallHangup(v1::CallHangup {
            call_id: bytes(call.0),
            reason: failed.then(|| v1::CallEndReason::Failed.into()),
        }))
    }

    /// Sends one of this side's ICE candidates to the other party; after the last one,
    /// [`IceCandidate::end_of_candidates`].
    pub async fn send_ice(&self, call: CallId, candidate: IceCandidate) -> Result<(), SdkError> {
        check_candidate(&CandidateInput {
            candidate: &candidate.candidate,
            sdp_mid: candidate.sdp_mid.as_deref(),
            username_fragment: candidate.username_fragment.as_deref(),
            end_of_candidates: candidate.end_of_candidates,
        })
        .map_err(|error| SdkError::Invalid(error.to_string()))?;
        self.usable()?;
        self.send(Body::CallIce(v1::CallIce {
            call_id: bytes(call.0),
            candidate: candidate.candidate,
            sdp_mid: candidate.sdp_mid,
            sdp_mline_index: candidate.sdp_mline_index,
            end_of_candidates: candidate.end_of_candidates,
            username_fragment: candidate.username_fragment,
        }))
    }

    /// Sends a new offer or answer for an active call (an ICE restart after a
    /// reconnect, for one); the other party gets [`CallEvent::PeerRenegotiate`].
    ///
    /// [`CallEvent::PeerRenegotiate`]: crate::CallEvent::PeerRenegotiate
    pub async fn renegotiate(
        &self,
        call: CallId,
        sdp: &str,
        kind: SdpKind,
    ) -> Result<(), SdkError> {
        checked_sdp(sdp)?;
        self.usable()?;
        self.send(Body::CallRenegotiate(v1::CallRenegotiate {
            call_id: bytes(call.0),
            sdp: sdp.to_owned(),
            r#type: kind.to_wire().into(),
        }))
    }

    /// The STUN and TURN servers for a call, with short-lived TURN credentials. Needs a
    /// signed-in client only (not a connection, nor [`Calls::available`]).
    ///
    /// Fetch them for every call, or when fewer than 60 s are left before
    /// [`IceServers::expires_at_ms`]; give them unchanged to the peer connection
    /// (`RTCConfiguration.iceServers`), and to a running call with `setConfiguration`
    /// before an ICE restart. [`IceServers::relay`] false means no TURN server (calls
    /// between symmetric NATs fail). For IP privacy, use relay-only ICE
    /// (`iceTransportPolicy: "relay"`, only with `relay`).
    ///
    /// Errors: the server's `turn_unavailable` (retryable, `retry_after_s` 30) when no
    /// relay is configured or the provider failed; `rate_limited` with `retry_after_s`.
    ///
    /// [`IceServers::expires_at_ms`]: crate::IceServers::expires_at_ms
    /// [`IceServers::relay`]: crate::IceServers::relay
    pub async fn ice_servers(&self) -> Result<IceServers, SdkError> {
        let inner = self.client.inner.clone();
        self.client
            .run(async move {
                let session = inner.credentials.fresh().await?;
                inner.auth.ice_servers(&session.access_token).await
            })
            .await
    }

    /// A page of the user's call history, newest first (ended calls only; ringing and
    /// active ones are in [`CallEvent::Synced`]). Needs a signed-in client only.
    ///
    /// `before` and `since` are cursors ([`CallLogEntry::cursor`], [`CallPage::next`]),
    /// passed back unchanged: `before` pages back to older calls, `since` lists the calls
    /// newer than one already seen; they combine. `limit` is 1 to 100 (50 when `None`);
    /// anything else is [`SdkError::Invalid`]. For a missed-call badge, ask with `since`
    /// on [`CallEvent::HistoryChanged`] and after every reconnect (docs/calls.md §12).
    ///
    /// Errors: the server's `invalid` (a cursor it does not know), `rate_limited` with
    /// `retry_after_s`, `unavailable` (retryable).
    ///
    /// [`CallEvent::Synced`]: crate::CallEvent::Synced
    /// [`CallEvent::HistoryChanged`]: crate::CallEvent::HistoryChanged
    /// [`CallLogEntry::cursor`]: crate::CallLogEntry::cursor
    /// [`CallPage::next`]: crate::CallPage::next
    pub async fn history(
        &self,
        before: Option<&str>,
        since: Option<&str>,
        limit: Option<u32>,
    ) -> Result<CallPage, SdkError> {
        if limit.is_some_and(|limit| !(1..=100).contains(&limit)) {
            return Err(SdkError::Invalid(
                "the call history limit is 1 to 100".to_owned(),
            ));
        }
        let inner = self.client.inner.clone();
        let (before, since) = (before.map(str::to_owned), since.map(str::to_owned));
        self.client
            .run(async move {
                let session = inner.credentials.fresh().await?;
                inner
                    .auth
                    .call_history(
                        &session.access_token,
                        before.as_deref(),
                        since.as_deref(),
                        limit,
                    )
                    .await
            })
            .await
    }

    /// Hides a call from the user's history, on all their devices (the other participant
    /// still sees it). `Ok` whether or not the user had such a call. Needs a signed-in
    /// client only. Errors: the server's `rate_limited` with `retry_after_s`,
    /// `unavailable` (retryable).
    pub async fn hide(&self, call: CallId) -> Result<(), SdkError> {
        let inner = self.client.inner.clone();
        self.client
            .run(async move {
                let session = inner.credentials.fresh().await?;
                inner.auth.hide_call(&session.access_token, call).await
            })
            .await
    }

    /// A call frame may go now: a connection serves and calls are available on it. The
    /// state can still say `Connected` for a moment after the connection ended (while
    /// the token is refreshed, say): that is offline too.
    fn usable(&self) -> Result<(), SdkError> {
        match self.client.connection_state() {
            ConnectionState::Connected => {}
            ConnectionState::SignedOut => return Err(SdkError::NotSignedIn),
            ConnectionState::DeviceRemoved => return Err(SdkError::DeviceRemoved),
            ConnectionState::OpenedElsewhere => return Err(SdkError::OpenedElsewhere),
            _ => return Err(SdkError::Offline),
        }
        let Some(offered) = self.client.inner.shared.calls_offered() else {
            return Err(SdkError::Offline);
        };
        if !offered {
            return Err(SdkError::Server {
                code: "not_available".to_owned(),
                message: "calls are not offered on this connection".to_owned(),
                retryable: false,
                retry_after_s: None,
            });
        }
        Ok(())
    }

    fn send(&self, frame: Body) -> Result<(), SdkError> {
        self.client
            .inner
            .commands
            .send(Command::Frame(frame))
            .map_err(|_| SdkError::Closed)
    }
}

fn checked_sdp(sdp: &str) -> Result<(), SdkError> {
    check_sdp(sdp).map_err(|error| SdkError::Invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use push_domain::DeviceId;
    use push_proto::v1::Capability;

    use super::*;
    use crate::Platform;
    use crate::calls::CallEvent;
    use crate::client::tests::Fixture;
    use crate::model::Event;
    use crate::testing::{Server, close, recv, recv_until, send};

    const OFFER: &str =
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
    const CANDIDATE: &str = "candidate:1 1 udp 2122260223 192.0.2.1 54321 typ host";

    fn offered() -> Vec<i32> {
        vec![Capability::Calls.into()]
    }

    /// A client with calls on, connected to a gateway that offers them.
    async fn connected() -> (Fixture, Server) {
        let mut f = Fixture::with_calls().await;
        f.client.connect().unwrap();
        let (socket, _) = f.ready_with(offered()).await;
        f.client
            .wait_connected(Duration::from_secs(10))
            .await
            .unwrap();
        (f, socket)
    }

    fn is_call_frame(body: &Body) -> bool {
        matches!(
            body,
            Body::CallInvite(_)
                | Body::CallAccept(_)
                | Body::CallDecline(_)
                | Body::CallCancel(_)
                | Body::CallIce(_)
                | Body::CallHangup(_)
                | Body::CallRenegotiate(_)
        )
    }

    fn not_available(result: Result<(), SdkError>) -> bool {
        matches!(result, Err(SdkError::Server { code, retryable: false, .. }) if code == "not_available")
    }

    #[tokio::test]
    async fn identify_advertises_calls_only_when_the_config_asks() {
        let mut f = Fixture::new().await;
        f.client.connect().unwrap();
        let (_socket, identify) = f.ready_with(offered()).await;
        assert!(identify.capabilities.is_empty());
        f.client
            .wait_connected(Duration::from_secs(10))
            .await
            .unwrap();
        assert!(!f.client.calls().available(), "the client did not ask");

        let mut f = Fixture::with_calls().await;
        assert!(!f.client.calls().available(), "not connected");
        f.client.connect().unwrap();
        let (_socket, identify) = f.ready_with(offered()).await;
        assert_eq!(identify.capabilities, offered());
        f.client
            .wait_connected(Duration::from_secs(10))
            .await
            .unwrap();
        assert!(f.client.calls().available());

        f.client.disconnect().unwrap();
        while f.client.connection_state() != ConnectionState::Offline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!f.client.calls().available(), "the connection ended");
    }

    #[tokio::test]
    async fn calls_are_not_available_when_ready_does_not_offer_them() {
        let mut f = Fixture::with_calls().await;
        f.client.connect().unwrap();
        let (_socket, identify) = f.ready_with(Vec::new()).await;
        assert_eq!(identify.capabilities, offered());
        f.client
            .wait_connected(Duration::from_secs(10))
            .await
            .unwrap();
        let calls = f.client.calls();
        assert!(!calls.available());
        let call = CallId::new();
        let invited = calls.invite(UserId::new(), OFFER).await.map(|_| ());
        assert!(not_available(invited.clone()), "{invited:?}");
        assert!(not_available(calls.hangup(call, false).await));
        assert!(not_available(
            calls
                .send_ice(call, IceCandidate::end_of_candidates())
                .await
        ));
    }

    #[tokio::test]
    async fn call_frames_need_a_live_connection() {
        let f = Fixture::with_calls().await;
        let calls = f.client.calls();
        let call = CallId::new();
        let offline = |result: Result<(), SdkError>| result == Err(SdkError::Offline);
        assert!(offline(
            calls.invite(UserId::new(), OFFER).await.map(|_| ())
        ));
        assert!(offline(calls.accept(call, OFFER).await));
        assert!(offline(calls.decline(call, false).await));
        assert!(offline(calls.cancel(call).await));
        assert!(offline(calls.hangup(call, true).await));
        assert!(offline(
            calls
                .send_ice(call, IceCandidate::end_of_candidates())
                .await
        ));
        assert!(offline(
            calls.renegotiate(call, OFFER, SdpKind::Offer).await
        ));
    }

    #[tokio::test]
    async fn a_connection_that_ended_is_offline_for_calls_before_the_state_says_so() {
        let (f, mut socket) = connected().await;
        // The 4002 refresh stalls: the state stays Connected meanwhile.
        f.auth.refresh_delay_ms.store(10_000, Ordering::SeqCst);
        close(&mut socket, 4002).await;
        let refreshed = async {
            while f.auth.refreshes.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), refreshed)
            .await
            .expect("a refresh within 10 s");
        assert_eq!(f.client.connection_state(), ConnectionState::Connected);
        let calls = f.client.calls();
        assert!(!calls.available());
        assert_eq!(
            calls.hangup(CallId::new(), false).await,
            Err(SdkError::Offline)
        );
        assert_eq!(
            calls.invite(UserId::new(), OFFER).await.map(|_| ()),
            Err(SdkError::Offline)
        );
    }

    #[tokio::test]
    async fn a_reconnect_whose_ready_does_not_offer_calls_makes_them_unavailable() {
        let (mut f, socket) = connected().await;
        assert!(f.client.calls().available());
        drop(socket);
        let (_socket, _) = f.ready_with(Vec::new()).await;
        f.client
            .wait_connected(Duration::from_secs(10))
            .await
            .unwrap();
        let calls = f.client.calls();
        assert!(!calls.available());
        assert!(not_available(calls.cancel(CallId::new()).await));
    }

    #[tokio::test]
    async fn invalid_input_is_refused_as_such_while_offline() {
        let f = Fixture::with_calls().await;
        let calls = f.client.calls();
        let call = CallId::new();
        let invalid = |result: Result<(), SdkError>| matches!(result, Err(SdkError::Invalid(_)));
        assert!(invalid(
            calls.invite(UserId::new(), "v=0\r\n").await.map(|_| ())
        ));
        assert!(invalid(calls.accept(call, "").await));
        assert!(invalid(
            calls.renegotiate(call, "v=0\r\n", SdpKind::Offer).await
        ));
        assert!(invalid(calls.send_ice(call, IceCandidate::default()).await));
    }

    /// The next [`CallEvent::Synced`] after an empty `CallSync`.
    async fn synced_after_an_empty_sync(f: &mut Fixture, socket: &mut Server) -> CallEvent {
        send(socket, Body::CallSync(v1::CallSync { calls: Vec::new() })).await;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), f.events.next())
                .await
                .expect("an event within 10 s")
                .expect("the event stream is open");
            if let Event::Call(synced @ CallEvent::Synced { .. }) = event {
                return synced;
            }
        }
    }

    #[tokio::test]
    async fn signing_out_forgets_the_calls() {
        let (mut f, _socket) = connected().await;
        f.client.calls().invite(UserId::new(), OFFER).await.unwrap();
        f.client.sign_out().await.unwrap();
        assert!(!f.client.calls().available());
        f.client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        f.client.connect().unwrap();
        let (mut socket, _) = f.ready_with(offered()).await;
        assert_eq!(
            synced_after_an_empty_sync(&mut f, &mut socket).await,
            CallEvent::Synced {
                calls: Vec::new(),
                gone: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn another_account_does_not_inherit_the_calls() {
        let (mut f, _socket) = connected().await;
        f.client.calls().invite(UserId::new(), OFFER).await.unwrap();
        let (user_b, device_b) = (UserId::new(), DeviceId::new());
        *f.auth.identity.lock().unwrap() = (user_b, device_b);
        f.client
            .sign_in("b@example.com", "password", "tablet", Platform::Ios)
            .await
            .unwrap();
        assert!(!f.client.calls().available());
        f.client.connect().unwrap();
        let mut socket = f.gateway.next().await;
        let Body::Identify(_) = recv(&mut socket).await else {
            panic!("Identify first");
        };
        let ready = v1::Ready {
            user_id: bytes(user_b.0),
            device_id: bytes(device_b.0),
            heartbeat_ms: 30_000,
            capabilities: offered(),
        };
        send(&mut socket, Body::Ready(ready)).await;
        assert_eq!(
            synced_after_an_empty_sync(&mut f, &mut socket).await,
            CallEvent::Synced {
                calls: Vec::new(),
                gone: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn bad_input_is_refused_before_anything_is_sent() {
        let (f, _socket) = connected().await;
        let calls = f.client.calls();
        let call = CallId::new();
        let invalid = |result: Result<(), SdkError>| matches!(result, Err(SdkError::Invalid(_)));
        let oversize = format!(
            "{OFFER}a={}\r\n",
            "x".repeat(push_domain::calls::MAX_SDP_BYTES)
        );
        let video = format!("{OFFER}m=video 9 UDP/TLS/RTP/SAVPF 96\r\n");
        for sdp in ["v=0\r\ns=-\r\n", video.as_str(), oversize.as_str(), ""] {
            assert!(invalid(calls.invite(UserId::new(), sdp).await.map(|_| ())));
            assert!(invalid(calls.accept(call, sdp).await));
            assert!(invalid(calls.renegotiate(call, sdp, SdpKind::Answer).await));
        }
        let me = f.auth.user;
        assert!(invalid(calls.invite(me, OFFER).await.map(|_| ())), "self");
        for candidate in [
            IceCandidate::default(),
            IceCandidate {
                candidate: "not a candidate".to_owned(),
                ..IceCandidate::default()
            },
            IceCandidate {
                candidate: CANDIDATE.to_owned(),
                sdp_mid: Some("x".repeat(push_domain::calls::MAX_SDP_MID_BYTES + 1)),
                ..IceCandidate::default()
            },
        ] {
            assert!(invalid(calls.send_ice(call, candidate).await));
        }
    }

    #[tokio::test]
    async fn each_call_method_sends_its_frame() {
        let (mut f, mut socket) = connected().await;
        let calls = f.client.calls();
        let peer = UserId::new();
        let invited = calls.invite(peer, OFFER).await.unwrap();
        assert_eq!(invited.0.get_version_num(), 7);
        let call = CallId::new();
        let id = bytes(call.0);
        calls.accept(call, OFFER).await.unwrap();
        calls.decline(call, true).await.unwrap();
        calls.decline(call, false).await.unwrap();
        calls.cancel(call).await.unwrap();
        calls.hangup(call, true).await.unwrap();
        calls.hangup(call, false).await.unwrap();
        let candidate = IceCandidate {
            candidate: CANDIDATE.to_owned(),
            sdp_mid: Some("0".to_owned()),
            sdp_mline_index: Some(0),
            username_fragment: Some("abcd".to_owned()),
            end_of_candidates: false,
        };
        calls.send_ice(call, candidate).await.unwrap();
        calls
            .send_ice(call, IceCandidate::end_of_candidates())
            .await
            .unwrap();
        calls
            .renegotiate(call, OFFER, SdpKind::Offer)
            .await
            .unwrap();
        calls
            .renegotiate(call, OFFER, SdpKind::Answer)
            .await
            .unwrap();
        let expected = [
            Body::CallInvite(v1::CallInvite {
                call_id: bytes(invited.0),
                callee_user_id: bytes(peer.0),
                offer_sdp: OFFER.to_owned(),
                media: vec![v1::CallMedia::Audio.into()],
            }),
            Body::CallAccept(v1::CallAccept {
                call_id: id.clone(),
                answer_sdp: OFFER.to_owned(),
            }),
            Body::CallDecline(v1::CallDecline {
                call_id: id.clone(),
                reason: Some(v1::CallEndReason::Busy.into()),
            }),
            Body::CallDecline(v1::CallDecline {
                call_id: id.clone(),
                reason: None,
            }),
            Body::CallCancel(v1::CallCancel {
                call_id: id.clone(),
            }),
            Body::CallHangup(v1::CallHangup {
                call_id: id.clone(),
                reason: Some(v1::CallEndReason::Failed.into()),
            }),
            Body::CallHangup(v1::CallHangup {
                call_id: id.clone(),
                reason: None,
            }),
            Body::CallIce(v1::CallIce {
                call_id: id.clone(),
                candidate: CANDIDATE.to_owned(),
                sdp_mid: Some("0".to_owned()),
                sdp_mline_index: Some(0),
                end_of_candidates: false,
                username_fragment: Some("abcd".to_owned()),
            }),
            Body::CallIce(v1::CallIce {
                call_id: id.clone(),
                candidate: String::new(),
                sdp_mid: None,
                sdp_mline_index: None,
                end_of_candidates: true,
                username_fragment: None,
            }),
            Body::CallRenegotiate(v1::CallRenegotiate {
                call_id: id.clone(),
                sdp: OFFER.to_owned(),
                r#type: v1::CallSdpType::Offer.into(),
            }),
            Body::CallRenegotiate(v1::CallRenegotiate {
                call_id: id,
                sdp: OFFER.to_owned(),
                r#type: v1::CallSdpType::Answer.into(),
            }),
        ];
        let mut sent = Vec::new();
        while sent.len() < expected.len() {
            sent.push(recv_until(&mut socket, is_call_frame).await);
        }
        assert_eq!(sent, expected);

        // The calls invited and accepted here are live. The connection's first CallSync
        // went out before the gateway read them, so it cannot list them; a later one
        // without them says they are gone.
        assert_eq!(
            synced_after_an_empty_sync(&mut f, &mut socket).await,
            CallEvent::Synced {
                calls: Vec::new(),
                gone: Vec::new(),
            }
        );
        let mut gone = vec![invited, call];
        gone.sort();
        assert_eq!(
            synced_after_an_empty_sync(&mut f, &mut socket).await,
            CallEvent::Synced {
                calls: Vec::new(),
                gone,
            }
        );
    }

    #[test]
    fn ice_candidates_do_not_show_their_text_in_debug() {
        let candidate = IceCandidate {
            candidate: CANDIDATE.to_owned(),
            sdp_mid: Some("0".to_owned()),
            sdp_mline_index: Some(0),
            username_fragment: Some("secret-ufrag".to_owned()),
            end_of_candidates: false,
        };
        let shown = format!("{candidate:?}");
        assert!(
            !shown.contains("192.0.2.1") && !shown.contains("secret-ufrag"),
            "{shown}"
        );
        let end = IceCandidate::end_of_candidates();
        assert!(end.end_of_candidates && end.candidate.is_empty());
    }

    #[tokio::test]
    async fn a_history_limit_out_of_range_is_refused_locally() {
        let f = Fixture::new().await;
        for limit in [0, 101] {
            let result = f.client.calls().history(None, None, Some(limit)).await;
            assert!(matches!(result, Err(SdkError::Invalid(_))), "{result:?}");
        }
    }
}
