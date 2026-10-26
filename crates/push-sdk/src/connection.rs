//! The connection to the gateway (spec §7.3, §7.4): one task that connects, identifies
//! with the device's access token, serves the connection until it ends, and decides what
//! comes next from the close code:
//!
//! - 4001 (the token is invalid or older than a password reset): forget the credentials,
//!   report `SignedOut` and stop;
//! - 4003 (the device was revoked): report `DeviceRemoved` and stop, keeping the
//!   credentials;
//! - 4007 (another connection of the device took over: the same app data opened
//!   elsewhere): report `OpenedElsewhere` and stop, keeping the credentials;
//! - 4002 (the token expired): refresh it and reconnect at once;
//! - 4006 (the device is not relayed yet): reconnect within a few seconds, never sign out;
//! - anything else (4004, 4005, 1002, 1008, 1009, 1011, a network error), and a delivery
//!   the store could not take: reconnect with exponential backoff and jitter, 1 s up to
//!   60 s.
//!
//! Local data is never erased here: not on a sign-out by the server, a removal or a
//! takeover (owner decision).
//!
//! While connected it pings every heartbeat (the first ping at a random point of the
//! first one), refreshes the token and sends `ReAuth` when the gateway announces the
//! expiry, keeps its own frame budget below the gateway's, and lets the [`Engine`] handle
//! deliveries and the outbox.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, Identify, Ping, ReAuth, request, response};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::SdkError;
use crate::credentials::Credentials;
use crate::engine::{Engine, Shared};
use crate::model::{ConnectionState, Event};
use crate::policy::{AfterClose, Backoff, FrameBudget, after_close, first_ping};
use crate::secrets::Session;

/// How long opening the socket may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the gateway may take to answer `Identify`.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a `Ping` may wait for its `Pong` before the connection counts as dead.
const PONG_TIMEOUT: Duration = Duration::from_secs(15);
/// How long sending one frame may take.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// A connection that stayed up this long resets the backoff.
const STABLE: Duration = Duration::from_secs(10);
/// How soon a refused `ReAuth` (the gateway could not check it) or a failed refresh is
/// tried again; the gateway closes with 4002 at `exp + 30 s` regardless.
const REAUTH_RETRY: Duration = Duration::from_secs(1);
const REFRESH_RETRY: Duration = Duration::from_secs(5);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Reply = oneshot::Sender<Result<response::Body, SdkError>>;

/// What the app asks of the connection.
pub(crate) enum Command {
    /// Connect, and keep reconnecting.
    Connect,
    /// Close the connection and stop reconnecting.
    Disconnect,
    /// Like `Disconnect`, and fires the sender once the connection has ended and the task
    /// will not touch the store again, so the caller may erase or rebind it.
    Stop(oneshot::Sender<()>),
    /// The outbox or a read position changed.
    Wake,
    /// A request (spec §7.1); answered with its `Response`, or an error.
    Request { body: request::Body, reply: Reply },
    /// A frame nothing answers (activity): sent while a connection is live, dropped
    /// otherwise, never queued for a later connection.
    Frame(Body),
}

/// Where the gateway is.
#[derive(Clone)]
pub(crate) struct Endpoint {
    /// `wss://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header when the URL names a load balancer rather than the virtual host.
    pub host: Option<String>,
}

/// How a connection ended.
enum Ended {
    /// The app disconnected (and wants to know when it is done: a `Stop`).
    Disconnected(Option<oneshot::Sender<()>>),
    /// The client is gone.
    Closed,
    /// No credentials, or auth-svc refused them.
    SignedOut,
    Lost {
        after: AfterClose,
        /// How long it was `Ready`, if it got there.
        ready_for: Option<Duration>,
        /// The access token it used.
        token: String,
    },
}

/// Moves the connection state to `state` and, if it changed, reports it. The event is
/// queued while the state is held, so the client's `connect` and the connection task
/// report changes in the order they made them.
pub(crate) fn set_state(
    sender: &watch::Sender<ConnectionState>,
    shared: &Shared,
    state: ConnectionState,
) {
    sender.send_if_modified(|current| {
        if *current == state {
            return false;
        }
        *current = state;
        shared.emit(Event::ConnectionStateChanged(state));
        true
    });
}

/// The connection task.
pub(crate) struct Connection {
    shared: Arc<Shared>,
    credentials: Arc<Credentials>,
    endpoint: Endpoint,
    state: Arc<watch::Sender<ConnectionState>>,
    commands: mpsc::UnboundedReceiver<Command>,
    engine: Engine,
    pong_timeout: Duration,
    /// How long a connection must stay up to reset the backoff.
    stable: Duration,
}

impl Connection {
    pub fn new(
        shared: Arc<Shared>,
        credentials: Arc<Credentials>,
        endpoint: Endpoint,
        state: Arc<watch::Sender<ConnectionState>>,
        commands: mpsc::UnboundedReceiver<Command>,
    ) -> Self {
        Self {
            engine: Engine::new(shared.clone()),
            shared,
            credentials,
            endpoint,
            state,
            commands,
            pong_timeout: PONG_TIMEOUT,
            stable: STABLE,
        }
    }

    fn set_state(&self, state: ConnectionState) {
        set_state(&self.state, &self.shared, state);
    }

    fn signed_out(&self) {
        self.set_state(ConnectionState::SignedOut);
        self.shared.emit(Event::SignedOut);
    }

    /// Runs until the client is dropped.
    pub async fn run(mut self) {
        let mut wanted = false;
        let mut attempts = 0u32;
        let mut not_ready = 0u32;
        let mut refreshed = false;
        loop {
            if !wanted {
                match self.commands.recv().await {
                    None => return,
                    Some(Command::Connect) => {
                        wanted = true;
                        attempts = 0;
                        not_ready = 0;
                    }
                    Some(Command::Request { reply, .. }) => {
                        let _ = reply.send(Err(SdkError::Offline));
                        continue;
                    }
                    Some(Command::Stop(ack)) => {
                        let _ = ack.send(());
                        continue;
                    }
                    Some(Command::Disconnect | Command::Wake | Command::Frame(_)) => continue,
                }
            }
            self.set_state(ConnectionState::Connecting);
            let (after, ready_for, token) = match self.connect_and_serve().await {
                Ended::Closed => return,
                Ended::Disconnected(ack) => {
                    wanted = false;
                    self.set_state(ConnectionState::Offline);
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                    continue;
                }
                Ended::SignedOut => {
                    wanted = false;
                    self.signed_out();
                    continue;
                }
                Ended::Lost {
                    after,
                    ready_for,
                    token,
                } => (after, ready_for, token),
            };
            if ready_for.is_some_and(|ready_for| ready_for >= self.stable) {
                attempts = 0;
                not_ready = 0;
                refreshed = false;
            }
            let delay = match after {
                AfterClose::SignOut => {
                    if let Err(error) = self.credentials.sign_out().await {
                        tracing::warn!(code = error.code(), "could not forget the credentials");
                    }
                    wanted = false;
                    self.signed_out();
                    continue;
                }
                AfterClose::DeviceRemoved => {
                    wanted = false;
                    self.set_state(ConnectionState::DeviceRemoved);
                    self.shared.emit(Event::DeviceRemoved);
                    continue;
                }
                AfterClose::OpenedElsewhere => {
                    wanted = false;
                    self.set_state(ConnectionState::OpenedElsewhere);
                    self.shared.emit(Event::OpenedElsewhere);
                    continue;
                }
                AfterClose::Refresh => match self.credentials.refresh(&token).await {
                    // Once at once; a second 4002 in a row backs off (a skewed clock).
                    Ok(_) if !refreshed => {
                        refreshed = true;
                        Duration::ZERO
                    }
                    Err(SdkError::NotSignedIn) => {
                        wanted = false;
                        self.signed_out();
                        continue;
                    }
                    _ => Backoff::RECONNECT.delay(next(&mut attempts)),
                },
                AfterClose::NotReadyYet => Backoff::NOT_READY.delay(next(&mut not_ready)),
                AfterClose::Reconnect => Backoff::RECONNECT.delay(next(&mut attempts)),
            };
            if after != AfterClose::Refresh {
                refreshed = false;
            }
            if delay.is_zero() {
                continue;
            }
            self.set_state(ConnectionState::Reconnecting {
                attempt: attempts + not_ready,
                retry_in_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            });
            let deadline = Instant::now() + delay;
            loop {
                tokio::select! {
                    () = sleep_until(deadline) => break,
                    command = self.commands.recv() => match command {
                        None => return,
                        Some(Command::Connect) => break,
                        Some(Command::Disconnect) => {
                            wanted = false;
                            self.set_state(ConnectionState::Offline);
                            break;
                        }
                        Some(Command::Stop(ack)) => {
                            wanted = false;
                            self.set_state(ConnectionState::Offline);
                            let _ = ack.send(());
                            break;
                        }
                        Some(Command::Request { reply, .. }) => {
                            let _ = reply.send(Err(SdkError::Offline));
                        }
                        Some(Command::Wake | Command::Frame(_)) => {}
                    },
                }
            }
        }
    }

    /// The handshake runs where a `Stop` or `Disconnect` can end it at once: until the
    /// connection serves, nothing of it is worth keeping, and a `Stop` must not wait for
    /// a slow gateway.
    async fn connect_and_serve(&mut self) -> Ended {
        let mut handshake = Box::pin(Self::handshake(
            self.credentials.clone(),
            self.shared.clone(),
            self.endpoint.clone(),
        ));
        let shaken = loop {
            tokio::select! {
                shaken = &mut handshake => break shaken,
                command = self.commands.recv() => match command {
                    None => return Ended::Closed,
                    Some(Command::Disconnect) => return Ended::Disconnected(None),
                    Some(Command::Stop(ack)) => {
                        // Dropped first: nothing of it runs after the acknowledgement.
                        drop(handshake);
                        return Ended::Disconnected(Some(ack));
                    }
                    Some(Command::Request { reply, .. }) => {
                        let _ = reply.send(Err(SdkError::Offline));
                    }
                    Some(Command::Connect | Command::Wake | Command::Frame(_)) => {}
                },
            }
        };
        match shaken {
            Ok((socket, session, ready)) => self.serve(socket, &session, &ready).await,
            Err(ended) => ended,
        }
    }

    /// Gets a session, binds the store to its user, opens the socket and identifies.
    /// Holds no reference to the task, so it can be dropped at any await.
    async fn handshake(
        credentials: Arc<Credentials>,
        shared: Arc<Shared>,
        endpoint: Endpoint,
    ) -> Result<(Socket, Session, v1::Ready), Ended> {
        let session = match credentials.fresh().await {
            Ok(session) => session,
            Err(SdkError::NotSignedIn) => return Err(Ended::SignedOut),
            Err(error) => {
                tracing::info!(code = error.code(), "could not get an access token");
                return Err(lost(AfterClose::Reconnect, None, String::new()));
            }
        };
        let (user, device) = (session.user_id, session.device_id);
        let (bind_shared, bind_credentials) = (shared, credentials.clone());
        // Whole-file work (an erase may truncate the log): off the async workers. The
        // session is checked again under the store's lock: a sign-out or sign-in that
        // changed it has wiped or is about to rebind the store, and a bind for the old
        // session must not come after that.
        let bound = tokio::task::spawn_blocking(move || {
            let mut store = bind_shared.store();
            match bind_credentials.session() {
                Ok(Some(current)) if current.user_id == user && current.device_id == device => {
                    store.bind_owner(user).map(Some)
                }
                Ok(_) => Ok(None),
                Err(error) => Err(error),
            }
        })
        .await
        .unwrap_or_else(|_| Err(SdkError::Storage("the store task ended".to_owned())));
        match bound {
            Ok(Some(true)) => {
                tracing::info!("another user signed in: the local store was emptied");
            }
            Ok(Some(false)) => {}
            Ok(None) => {
                tracing::info!("the session changed meanwhile");
                return Err(lost(AfterClose::Reconnect, None, session.access_token));
            }
            Err(error) => {
                tracing::warn!(code = error.code(), "could not open the local store");
                return Err(lost(AfterClose::Reconnect, None, session.access_token));
            }
        }
        let token = session.access_token.clone();
        let mut socket = match timeout(CONNECT_TIMEOUT, open(&endpoint)).await {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => {
                tracing::info!(%error, "could not connect");
                return Err(lost(AfterClose::Reconnect, None, token));
            }
            Err(_) => {
                tracing::info!("connecting timed out");
                return Err(lost(AfterClose::Reconnect, None, token));
            }
        };
        let identify = Body::Identify(Identify {
            token: token.clone(),
        });
        if send_now(&mut socket, identify).await.is_err() {
            return Err(lost(AfterClose::Reconnect, None, token));
        }
        let ready = match timeout(IDENTIFY_TIMEOUT, first_answer(&mut socket)).await {
            Ok(Ok(ready)) => ready,
            Ok(Err(code)) => return Err(lost(after_close(code), None, token)),
            Err(_) => {
                tracing::info!("no answer to Identify");
                return Err(lost(AfterClose::Reconnect, None, token));
            }
        };
        if ready.user_id != id_bytes(session.user_id.0)
            || ready.device_id != id_bytes(session.device_id.0)
        {
            tracing::warn!("Ready named another user or device than the token");
            return Err(lost(AfterClose::Reconnect, None, token));
        }
        // After all the awaits: a session that changed meanwhile must not be served.
        let unchanged = credentials.session().is_ok_and(|current| {
            current.is_some_and(|current| current.user_id == user && current.device_id == device)
        });
        if !unchanged {
            tracing::info!("the session changed meanwhile");
            return Err(lost(AfterClose::Reconnect, None, token));
        }
        Ok((socket, session, ready))
    }

    async fn serve(&mut self, socket: Socket, session: &Session, ready: &v1::Ready) -> Ended {
        let ready_at = Instant::now();
        let heartbeat = Duration::from_millis(u64::from(ready.heartbeat_ms).clamp(1_000, 600_000));
        let mut live = Live {
            socket,
            budget: FrameBudget::new(ready_at),
            pending: HashMap::new(),
            next_request: 1,
            ping_at: ready_at + first_ping(heartbeat),
            pong_by: None,
            token: session.access_token.clone(),
            refresh: None,
            refresh_at: None,
            reauth_at: None,
        };
        self.set_state(ConnectionState::Connected);
        let mut out = Vec::new();
        self.engine.start(unix_ms(), &mut out);
        let after = loop {
            if live.send_ready(&mut out).await.is_err() {
                break AfterClose::Reconnect;
            }
            // Frames over the budget wait here, while the socket is still read.
            let send_at =
                (!out.is_empty()).then(|| Instant::now() + live.budget.wait(Instant::now()));
            let acks_at = if out.is_empty() {
                self.engine.acks_due()
            } else {
                None
            };
            let more_reads = out.is_empty() && self.engine.backlog();
            let retry_at = self.engine.retry_due(unix_ms()).map(|due| {
                Instant::now() + Duration::from_millis(u64::try_from(due - unix_ms()).unwrap_or(0))
            });
            tokio::select! {
                message = live.socket.next() => match message {
                    Some(Ok(Message::Binary(bytes))) => match decode_frame(&bytes) {
                        Ok(body) => {
                            let back = self.engine.on_frame(body, Instant::now(), unix_ms(), &mut out);
                            if self.engine.broken() {
                                break AfterClose::Reconnect;
                            }
                            if let Some(body) = back {
                                if let Some(ended) = self.on_own_frame(&mut live, body, &mut out) {
                                    break ended;
                                }
                            }
                        }
                        Err(error) => tracing::warn!(%error, "an unreadable frame"),
                    },
                    Some(Ok(Message::Close(frame))) => {
                        break after_close(frame.map(|frame| u16::from(frame.code)));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::info!(%error, "the connection failed");
                        break AfterClose::Reconnect;
                    }
                    None => break AfterClose::Reconnect,
                },
                command = self.commands.recv() => match command {
                    None => {
                        live.close().await;
                        self.end(&mut live);
                        return Ended::Closed;
                    }
                    Some(Command::Disconnect) => {
                        live.close().await;
                        self.end(&mut live);
                        return Ended::Disconnected(None);
                    }
                    Some(Command::Stop(ack)) => {
                        live.close().await;
                        self.end(&mut live);
                        return Ended::Disconnected(Some(ack));
                    }
                    Some(Command::Connect) => {}
                    Some(Command::Wake) => self.engine.wake(unix_ms(), &mut out),
                    Some(Command::Frame(body)) => out.push(body),
                    Some(Command::Request { body, reply }) => {
                        let request_id = live.next_request;
                        live.next_request += 1;
                        live.pending.insert(request_id, reply);
                        out.push(Body::Request(v1::Request { request_id, body: Some(body) }));
                    }
                },
                () = sleep_until(live.ping_at) => {
                    // First in line: a long run of frames waiting for the budget must not
                    // delay the Ping that the Pong deadline counts from.
                    out.insert(0, Body::Ping(Ping {}));
                    live.ping_at = Instant::now() + heartbeat;
                    live.pong_by.get_or_insert(Instant::now() + self.pong_timeout);
                }
                () = sleep_until(send_at.unwrap_or_else(far_future)), if send_at.is_some() => {}
                () = std::future::ready(()), if more_reads => {
                    self.engine.wake(unix_ms(), &mut out);
                }
                () = sleep_until(live.pong_by.unwrap_or_else(far_future)), if live.pong_by.is_some() => {
                    tracing::info!("no Pong: the connection is dead");
                    break AfterClose::Reconnect;
                }
                () = sleep_until(acks_at.unwrap_or_else(far_future)), if acks_at.is_some() => {
                    self.engine.flush_acks(&mut out);
                }
                () = sleep_until(retry_at.unwrap_or_else(far_future)), if retry_at.is_some() => {
                    self.engine.wake(unix_ms(), &mut out);
                }
                () = sleep_until(live.refresh_at.unwrap_or_else(far_future)), if live.refresh_at.is_some() => {
                    live.refresh_at = None;
                    let (credentials, token) = (self.credentials.clone(), live.token.clone());
                    live.refresh = Some(tokio::spawn(async move { credentials.refresh(&token).await }));
                }
                () = sleep_until(live.reauth_at.unwrap_or_else(far_future)), if live.reauth_at.is_some() => {
                    live.reauth_at = None;
                    out.push(Body::ReAuth(ReAuth { token: live.token.clone() }));
                }
                refreshed = async { live.refresh.as_mut().expect("guarded").await }, if live.refresh.is_some() => {
                    live.refresh = None;
                    match refreshed {
                        Ok(Ok(fresh)) => {
                            live.token = fresh.access_token;
                            out.push(Body::ReAuth(ReAuth { token: live.token.clone() }));
                        }
                        Ok(Err(SdkError::NotSignedIn)) => {
                            live.close().await;
                            self.end(&mut live);
                            return Ended::SignedOut;
                        }
                        Ok(Err(error)) => {
                            tracing::info!(code = error.code(), "the refresh failed; trying again");
                            live.refresh_at = Some(Instant::now() + REFRESH_RETRY);
                        }
                        Err(_) => live.refresh_at = Some(Instant::now() + REFRESH_RETRY),
                    }
                }
            }
        };
        self.end(&mut live);
        Ended::Lost {
            after,
            ready_for: Some(ready_at.elapsed()),
            token: live.token,
        }
    }

    /// A frame the engine handed back: answers to the app's requests, the token's expiry,
    /// `Pong`. `Some` when the connection must end.
    fn on_own_frame(
        &mut self,
        live: &mut Live,
        body: Body,
        _out: &mut [Body],
    ) -> Option<AfterClose> {
        match body {
            Body::Response(answer) => {
                if let Some(reply) = live.pending.remove(&answer.request_id) {
                    let _ = reply.send(answer.body.ok_or_else(|| {
                        SdkError::UnexpectedAnswer(
                            "the server answered a request with nothing".to_owned(),
                        )
                    }));
                }
            }
            Body::Error(error) => match error.request_id {
                Some(request_id) => {
                    if let Some(reply) = live.pending.remove(&request_id) {
                        let _ = reply.send(Err(SdkError::from_frame(&error)));
                    }
                }
                // The only frame answered with an anonymous retryable error is ReAuth,
                // when the gateway cannot check it now.
                None if error.retryable => live.reauth_at = Some(Instant::now() + REAUTH_RETRY),
                None => tracing::info!(code = ?error.code(), "the gateway reported an error"),
            },
            Body::AuthExpiring(_) => {
                if live.refresh.is_none() {
                    live.refresh_at = Some(Instant::now());
                }
            }
            Body::Pong(_) => live.pong_by = None,
            _ => {}
        }
        None
    }

    /// The connection ended: rows in flight go again on the next one, and requests
    /// waiting for an answer learn it will not come. A refresh in flight is left to
    /// finish: auth-svc may have rotated the token already, and the refresh saves the new
    /// one; aborting it could lose the only refresh token that still works.
    fn end(&mut self, live: &mut Live) {
        self.engine.stop();
        for (_, reply) in live.pending.drain() {
            let _ = reply.send(Err(SdkError::Disconnected));
        }
        // Dropping the handle detaches the task.
        drop(live.refresh.take());
    }
}

/// An identified connection.
struct Live {
    socket: Socket,
    budget: FrameBudget,
    pending: HashMap<u64, Reply>,
    next_request: u64,
    ping_at: Instant,
    /// When the `Pong` for the last `Ping` is due.
    pong_by: Option<Instant>,
    /// The access token the connection holds now.
    token: String,
    refresh: Option<JoinHandle<Result<Session, SdkError>>>,
    refresh_at: Option<Instant>,
    reauth_at: Option<Instant>,
}

impl Live {
    /// Sends the frames in order while the client's frame budget allows; the rest stay in
    /// `out` for the next turn, so the connection keeps reading while it waits for budget.
    async fn send_ready(&mut self, out: &mut Vec<Body>) -> Result<(), ()> {
        let mut ready = 0;
        while ready < out.len() && self.budget.wait(Instant::now()).is_zero() {
            self.budget.spend(Instant::now());
            ready += 1;
        }
        for body in out.drain(..ready) {
            send_now(&mut self.socket, body).await?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        };
        let _ = timeout(Duration::from_secs(2), self.socket.close(Some(frame))).await;
    }
}

fn lost(after: AfterClose, ready_for: Option<Duration>, token: String) -> Ended {
    Ended::Lost {
        after,
        ready_for,
        token,
    }
}

/// The attempt number to use, counting it.
fn next(counter: &mut u32) -> u32 {
    let attempt = *counter;
    *counter = counter.saturating_add(1);
    attempt
}

async fn open(endpoint: &Endpoint) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = endpoint.socket_url.as_str().into_client_request()?;
    if let Some(host) = &endpoint.host {
        let value = HeaderValue::from_str(host)
            .map_err(|error| tokio_tungstenite::tungstenite::Error::HttpFormat(error.into()))?;
        request.headers_mut().insert("Host", value);
    }
    let (socket, _) = connect_async(request).await?;
    Ok(socket)
}

async fn send_now(socket: &mut Socket, body: Body) -> Result<(), ()> {
    match timeout(
        SEND_TIMEOUT,
        socket.send(Message::Binary(encode_frame(body).into())),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// The answer to `Identify`: `Ready`, or the close code (`None` without one).
async fn first_answer(socket: &mut Socket) -> Result<v1::Ready, Option<u16>> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                return match decode_frame(&bytes) {
                    Ok(Body::Ready(ready)) => Ok(ready),
                    _ => Err(None),
                };
            }
            Some(Ok(Message::Close(frame))) => return Err(frame.map(|frame| u16::from(frame.code))),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Err(None),
        }
    }
}

fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(24 * 60 * 60)
}

pub(crate) fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use push_proto::v1::{AuthExpiring, ListContacts, Pong, Ready};
    use tokio::time::sleep;

    use super::*;
    use crate::MemorySecretStore;
    use crate::auth::AuthApi;
    use crate::credentials::unix_now;
    use crate::secrets::SecretStore;
    use crate::store::Store;
    use crate::testing::{FakeAuth, FakeGateway, Server, close, recv, recv_until, send};

    struct Client {
        commands: mpsc::UnboundedSender<Command>,
        shared: Arc<Shared>,
        state: watch::Receiver<ConnectionState>,
        events: mpsc::Receiver<Event>,
        secrets: Arc<MemorySecretStore>,
        session: Session,
        refreshes: Arc<AtomicUsize>,
        refresh_delay_ms: Arc<AtomicU64>,
    }

    impl Client {
        async fn wait_for(&mut self, state: ConnectionState) {
            timeout(
                Duration::from_secs(10),
                self.state.wait_for(|now| *now == state),
            )
            .await
            .unwrap_or_else(|_| panic!("no {state:?} within 10 s"))
            .unwrap();
        }

        fn events(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }

        async fn request(
            &self,
            body: request::Body,
        ) -> oneshot::Receiver<Result<response::Body, SdkError>> {
            let (reply, answer) = oneshot::channel();
            self.commands
                .send(Command::Request { body, reply })
                .unwrap();
            answer
        }
    }

    /// A signed-in client of `gateway`, connecting; refreshes answer `access-N`.
    async fn connect(gateway: &FakeGateway) -> Client {
        connect_with(gateway, |_, _| {}, |_| {}).await
    }

    /// Like [`connect`]; `fill` prepares the store (owned by the user) and `tune` the
    /// connection before it starts.
    async fn connect_with(
        gateway: &FakeGateway,
        fill: impl FnOnce(&mut Store, push_domain::UserId),
        tune: impl FnOnce(&mut Connection),
    ) -> Client {
        let auth = FakeAuth::start().await;
        let session = Session {
            user_id: auth.user,
            device_id: auth.device,
            access_token: "access-0".to_owned(),
            access_expires_at: unix_now() + 900,
            refresh_token: "refresh-0".to_owned(),
        };
        let auth_url = auth.url.clone();
        let (refreshes, refresh_delay_ms) = (auth.refreshes.clone(), auth.refresh_delay_ms.clone());
        let secrets = Arc::new(MemorySecretStore::new());
        session.save(secrets.as_ref()).unwrap();
        let credentials = Arc::new(Credentials::new(
            secrets.clone() as Arc<dyn SecretStore>,
            AuthApi::new(&auth_url, None).unwrap(),
        ));
        let (event_sender, events) = mpsc::channel(crate::engine::EVENT_QUEUE);
        let mut store = Store::open(":memory:", None).unwrap();
        store.bind_owner(session.user_id).unwrap();
        fill(&mut store, session.user_id);
        let shared = Arc::new(Shared::new(store, event_sender));
        let handle = shared.clone();
        let (state_sender, state) = watch::channel(ConnectionState::Offline);
        let state_sender = Arc::new(state_sender);
        let (commands, receiver) = mpsc::unbounded_channel();
        let endpoint = Endpoint {
            socket_url: gateway.url.clone(),
            host: None,
        };
        let mut connection = Connection::new(shared, credentials, endpoint, state_sender, receiver);
        tune(&mut connection);
        tokio::spawn(connection.run());
        commands.send(Command::Connect).unwrap();
        Client {
            commands,
            shared: handle,
            state,
            events,
            secrets,
            session,
            refreshes,
            refresh_delay_ms,
        }
    }

    /// Reads `Identify` and answers `Ready`; the token it carried.
    async fn identify(client: &Client, socket: &mut Server, heartbeat_ms: u32) -> String {
        let Body::Identify(identify) = recv(socket).await else {
            panic!("the first frame is Identify");
        };
        send(
            socket,
            Body::Ready(Ready {
                user_id: id_bytes(client.session.user_id.0),
                device_id: id_bytes(client.session.device_id.0),
                heartbeat_ms,
            }),
        )
        .await;
        identify.token
    }

    #[tokio::test]
    async fn it_identifies_with_the_access_token_and_lists_once_ready() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        assert_eq!(identify(&client, &mut socket, 30_000).await, "access-0");
        client.wait_for(ConnectionState::Connected).await;
        let listings: Vec<_> = [recv(&mut socket).await, recv(&mut socket).await]
            .into_iter()
            .map(|body| match body {
                Body::Request(request) => request.body,
                _ => None,
            })
            .collect();
        assert!(matches!(
            &listings[..],
            [
                Some(request::Body::ListConversations(_)),
                Some(request::Body::ListContacts(_))
            ]
        ));
    }

    #[tokio::test]
    async fn not_ready_yet_is_retried_and_never_signs_out() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        for _ in 0..2 {
            let mut socket = gateway.next().await;
            recv(&mut socket).await;
            close(&mut socket, 4006).await;
        }
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        let events = client.events();
        assert!(!events.contains(&Event::SignedOut));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::ConnectionStateChanged(ConnectionState::Reconnecting { .. })
        )));
        assert!(Session::load(client.secrets.as_ref()).unwrap().is_some());
    }

    #[tokio::test]
    async fn an_invalid_identity_signs_out_and_stops() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        recv(&mut socket).await;
        close(&mut socket, 4001).await;
        client.wait_for(ConnectionState::SignedOut).await;
        assert!(client.events().contains(&Event::SignedOut));
        assert_eq!(Session::load(client.secrets.as_ref()).unwrap(), None);
        gateway.quiet(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn a_removed_device_stops_and_keeps_its_credentials() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        close(&mut socket, 4003).await;
        client.wait_for(ConnectionState::DeviceRemoved).await;
        assert!(client.events().contains(&Event::DeviceRemoved));
        assert!(Session::load(client.secrets.as_ref()).unwrap().is_some());
        gateway.quiet(Duration::from_secs(2)).await;
        // The app may try again; the gateway decides.
        client.commands.send(Command::Connect).unwrap();
        gateway.next().await;
    }

    #[tokio::test]
    async fn opened_elsewhere_stops_and_keeps_its_credentials() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        close(&mut socket, 4007).await;
        client.wait_for(ConnectionState::OpenedElsewhere).await;
        let events = client.events();
        assert!(events.contains(&Event::OpenedElsewhere));
        assert!(!events.contains(&Event::DeviceRemoved));
        assert!(Session::load(client.secrets.as_ref()).unwrap().is_some());
        gateway.quiet(Duration::from_secs(2)).await;
    }

    /// auth-svc rotates the refresh token when it answers; a refresh cut off by the end of
    /// the connection would lose the only refresh token that still works.
    #[tokio::test]
    async fn a_refresh_in_flight_finishes_when_the_connection_ends() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        client.refresh_delay_ms.store(300, Ordering::SeqCst);
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        send(
            &mut socket,
            Body::AuthExpiring(AuthExpiring { expires_in_s: 60 }),
        )
        .await;
        while client.refreshes.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(10)).await;
        }
        client.commands.send(Command::Disconnect).unwrap();
        client.wait_for(ConnectionState::Offline).await;
        sleep(Duration::from_millis(600)).await;
        let stored = Session::load(client.secrets.as_ref()).unwrap().unwrap();
        assert_eq!(stored.access_token, "access-1");
    }

    #[tokio::test]
    async fn an_expired_token_is_refreshed_before_reconnecting() {
        let mut gateway = FakeGateway::start().await;
        let client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        recv(&mut socket).await;
        close(&mut socket, 4002).await;
        let mut socket = gateway.next().await;
        assert_eq!(identify(&client, &mut socket, 30_000).await, "access-1");
        assert_eq!(client.refreshes.load(Ordering::SeqCst), 1);
        let stored = Session::load(client.secrets.as_ref()).unwrap().unwrap();
        assert_eq!(stored.access_token, "access-1");
    }

    #[tokio::test]
    async fn auth_expiring_refreshes_and_reauths_on_the_same_connection() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        send(
            &mut socket,
            Body::AuthExpiring(AuthExpiring { expires_in_s: 60 }),
        )
        .await;
        let reauth = loop {
            if let Body::ReAuth(reauth) = recv(&mut socket).await {
                break reauth;
            }
        };
        assert_eq!(reauth.token, "access-1");
    }

    #[tokio::test]
    async fn a_lost_connection_is_reconnected_and_pings_follow_the_heartbeat() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let socket = gateway.next().await;
        drop(socket);
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 1_000).await;
        client.wait_for(ConnectionState::Connected).await;
        let ping = loop {
            if let Body::Ping(ping) = recv(&mut socket).await {
                break ping;
            }
        };
        assert_eq!(ping, Ping {});
        send(&mut socket, Body::Pong(Pong {})).await;
    }

    #[tokio::test]
    async fn a_4002_after_a_long_healthy_connection_reconnects_at_once() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect_with(
            &gateway,
            |_, _| {},
            |connection| connection.stable = Duration::from_millis(200),
        )
        .await;
        // The first 4002 is answered at once and arms the back-off for a second one.
        let mut socket = gateway.next().await;
        recv(&mut socket).await;
        close(&mut socket, 4002).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        sleep(Duration::from_millis(400)).await;
        let closed_at = Instant::now();
        close(&mut socket, 4002).await;
        let mut socket = gateway.next().await;
        assert!(
            closed_at.elapsed() < Duration::from_millis(400),
            "no back-off after a healthy connection: {:?}",
            closed_at.elapsed()
        );
        assert_eq!(identify(&client, &mut socket, 30_000).await, "access-2");
    }

    #[tokio::test]
    async fn many_read_positions_do_not_starve_the_reads_of_the_connection() {
        const CONVERSATIONS: usize = 90;
        let mut gateway = FakeGateway::start().await;
        let client = connect_with(
            &gateway,
            |_, _| {},
            |connection| connection.pong_timeout = Duration::from_millis(1_500),
        )
        .await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 1_000).await;
        recv_until(&mut socket, |body| matches!(body, Body::Ping(_))).await;

        // The app reads everything at once, while the Pong is still on its way.
        {
            let mut store = client.shared.store();
            let peer = push_domain::UserId::new();
            for _ in 0..CONVERSATIONS {
                let conversation = push_domain::ConversationId::new();
                store.open_direct(conversation, peer).unwrap();
                let entry = crate::store::tests::entry(
                    conversation,
                    peer,
                    1,
                    push_proto::v1::EntryKind::Message,
                );
                store.apply_deliver(&entry).unwrap();
                store.mark_read(conversation, 1).unwrap();
            }
        }
        client.commands.send(Command::Wake).unwrap();
        sleep(Duration::from_millis(100)).await;
        send(&mut socket, Body::Pong(Pong {})).await;

        let mut reads = 0;
        let started = Instant::now();
        while reads < CONVERSATIONS {
            match recv(&mut socket).await {
                Body::ReadUpTo(_) => reads += 1,
                Body::Ping(_) => send(&mut socket, Body::Pong(Pong {})).await,
                _ => {}
            }
        }
        assert!(
            started.elapsed() > Duration::from_secs(2),
            "the frame budget held the rest back"
        );
        sleep(Duration::from_millis(300)).await;
        assert_eq!(*client.state.borrow(), ConnectionState::Connected);
        gateway.quiet(Duration::from_millis(300)).await;
    }

    #[tokio::test]
    async fn requests_are_answered_or_fail_when_the_connection_drops() {
        let mut gateway = FakeGateway::start().await;
        let mut client = connect(&gateway).await;
        let mut socket = gateway.next().await;
        identify(&client, &mut socket, 30_000).await;
        client.wait_for(ConnectionState::Connected).await;
        let answer = client
            .request(request::Body::ListContacts(ListContacts {}))
            .await;
        let request_id = loop {
            if let Body::Request(request) = recv(&mut socket).await {
                if request.request_id < crate::engine::INTERNAL_REQUESTS {
                    break request.request_id;
                }
            }
        };
        send(
            &mut socket,
            Body::Response(v1::Response {
                request_id,
                body: Some(response::Body::Done(v1::Done {})),
            }),
        )
        .await;
        assert_eq!(answer.await.unwrap(), Ok(response::Body::Done(v1::Done {})));

        let answer = client
            .request(request::Body::ListContacts(ListContacts {}))
            .await;
        loop {
            if let Body::Request(request) = recv(&mut socket).await {
                if request.request_id < crate::engine::INTERNAL_REQUESTS {
                    break;
                }
            }
        }
        drop(socket);
        assert_eq!(answer.await.unwrap(), Err(SdkError::Disconnected));
        client.commands.send(Command::Disconnect).unwrap();
        client.wait_for(ConnectionState::Offline).await;
    }
}
