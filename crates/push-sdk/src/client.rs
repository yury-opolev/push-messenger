//! The SDK's public face (spec §8.2): one [`Client`] per signed-in app profile, and the
//! [`Events`] stream it reports on.
//!
//! The client owns its own tokio runtime (spec §8.5), so it works the same from an async
//! Rust app, a Node binding or a C binding: its async methods run on the client's runtime
//! and can be awaited from any executor; its plain methods read or change the local store
//! at once. It is cheap to clone, and every clone is the same client.

mod presence;
mod requests;
mod sending;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use push_domain::{ConversationId, DeviceId, UserId};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot, watch};

use crate::SdkError;
use crate::auth::{AuthApi, Platform, Tokens};
use crate::connection::{Command, Connection, Endpoint, set_state};
use crate::credentials::{Credentials, MIN_ACCESS_LIFETIME_S, is_refused, unix_now};
use crate::engine::{EVENT_QUEUE, Shared};
use crate::key::DatabaseKey;
use crate::model::{ConnectionState, Contact, Conversation, Event, Message};
use crate::secrets::{SecretStore, Session};
use crate::store::Store;

/// How long after sending a message its sender may still edit or delete it: the server's
/// default (spec §4.6). The server checks again with its own setting.
pub const EDIT_WINDOW: Duration = Duration::from_secs(48 * 60 * 60);

/// Where the servers are, and where the client keeps its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// auth-svc's public URL: `https://push.example`.
    pub auth_url: String,
    /// The gateway's WebSocket: `wss://push.example/v1/socket`.
    pub socket_url: String,
    /// The `Host` header for both, when the URLs name an address rather than the virtual
    /// host a load balancer routes (a local cluster's ingress). Usually `None`.
    pub host: Option<String>,
    /// The SQLite file of this app profile; `:memory:` keeps nothing.
    pub database_path: String,
    /// Encrypts the whole database with this key (see [`DatabaseKey`]); `None` keeps it
    /// plain. The client uses it to open the file and keeps it only in SQLite's memory;
    /// the app provides it each time it opens the client.
    pub database_key: Option<DatabaseKey>,
    /// How long a request waits for the connection, and then for its answer.
    pub request_timeout: Duration,
    /// Allows `http://` and `ws://` URLs to hosts other than this machine (loopback,
    /// `localhost`, `*.localhost`, `*.local`, `*.test`), whose traffic — passwords,
    /// tokens and messages — anyone on the path can read. Off by default: only for
    /// development clusters reached over a private network.
    pub allow_insecure_transport: bool,
}

impl Config {
    pub fn new(auth_url: &str, socket_url: &str, database_path: &str) -> Self {
        Self {
            auth_url: auth_url.to_owned(),
            socket_url: socket_url.to_owned(),
            host: None,
            database_path: database_path.to_owned(),
            database_key: None,
            request_timeout: Duration::from_secs(15),
            allow_insecure_transport: false,
        }
    }

    /// Refuses URLs whose traffic would cross a network in plaintext, unless
    /// [`Self::allow_insecure_transport`]: `auth_url` must be `https://` and
    /// `socket_url` `wss://`, except to this machine.
    fn check_transport(&self) -> Result<(), SdkError> {
        check_url(
            "auth_url",
            &self.auth_url,
            "https",
            "http",
            self.allow_insecure_transport,
        )?;
        check_url(
            "socket_url",
            &self.socket_url,
            "wss",
            "ws",
            self.allow_insecure_transport,
        )
    }
}

fn check_url(
    field: &str,
    url: &str,
    secure: &str,
    plain: &str,
    allow_insecure: bool,
) -> Result<(), SdkError> {
    let parsed =
        reqwest::Url::parse(url).map_err(|error| SdkError::Invalid(format!("{field}: {error}")))?;
    let scheme = parsed.scheme();
    if scheme == secure {
        return Ok(());
    }
    if scheme != plain {
        return Err(SdkError::Invalid(format!(
            "{field} must be a {secure}:// URL"
        )));
    }
    if allow_insecure || parsed.host_str().is_some_and(is_this_machine) {
        return Ok(());
    }
    Err(SdkError::Invalid(format!(
        "{field} must be a {secure}:// URL: {plain}:// sends passwords, tokens and messages \
         in plaintext (set allow_insecure_transport only for a development cluster)"
    )))
}

/// Loopback addresses and the names reserved for this machine or local testing.
fn is_this_machine(host: &str) -> bool {
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(address) = unbracketed.parse::<std::net::IpAddr>() {
        return address.is_loopback();
    }
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || [".localhost", ".local", ".test"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Runs whole-file store work (an erase, a rekey, a rebind) on a blocking thread, so no
/// async worker waits on the file and the store's lock is never held across an await.
async fn blocking<T: Send + 'static>(
    shared: &Arc<Shared>,
    work: impl FnOnce(&mut Store) -> Result<T, SdkError> + Send + 'static,
) -> Result<T, SdkError> {
    let shared = Arc::clone(shared);
    tokio::task::spawn_blocking(move || work(&mut shared.store()))
        .await
        .unwrap_or_else(|_| Err(SdkError::Storage("the store task ended".to_owned())))
}

/// Aborts a task when dropped; a no-op for a task that ended.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs `future` as a task of its own, from inside a client's task: aborting the caller
/// does not stop it. For the steps that leave the store and the credentials inconsistent
/// if cut in half; each is bounded (a local write, or a stop that times out after
/// [`STOP_TIMEOUT`]).
async fn shielded<T: Send + 'static>(
    future: impl Future<Output = Result<T, SdkError>> + Send + 'static,
) -> Result<T, SdkError> {
    tokio::spawn(future).await.unwrap_or(Err(SdkError::Closed))
}

/// How long a sign-out, a sign-in or a close waits for the connection task to confirm it
/// stopped (closing a socket takes up to 2 s).
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Stops the connection and waits until its task has ended the connection and will not
/// touch the store again. [`SdkError::ConnectionBusy`] if that is not confirmed in time:
/// the caller must then not wipe or rebind the store.
async fn stop_connection(inner: &Inner) -> Result<(), SdkError> {
    let (ack, stopped) = oneshot::channel();
    inner
        .commands
        .send(Command::Stop(ack))
        .map_err(|_| SdkError::Closed)?;
    match tokio::time::timeout(STOP_TIMEOUT, stopped).await {
        // A dropped acknowledgement means the task itself is gone.
        Ok(_) => Ok(()),
        Err(_) => Err(SdkError::ConnectionBusy),
    }
}

/// Asks auth-svc to revoke the device of a session that is no longer stored. An access
/// token about to expire is refreshed in memory only, for this one call. A refusal means
/// the device's tokens are dead already.
async fn revoke(inner: &Inner, session: Session) -> Result<(), SdkError> {
    let access_token = if session.access_expires_at - unix_now() >= MIN_ACCESS_LIFETIME_S {
        session.access_token
    } else {
        match inner.auth.refresh(&session.refresh_token).await {
            Ok(tokens) => tokens.access_token,
            Err(error) if is_refused(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
    };
    match inner
        .auth
        .revoke_device(&access_token, session.device_id)
        .await
    {
        Err(error) if is_refused(&error) => Ok(()),
        revoked => revoked,
    }
}

/// The signed-in user and device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Account {
    pub user_id: UserId,
    pub device_id: DeviceId,
}

/// The client's events, in order (spec §8.2). Bindings forward them to the app's
/// callback; a Rust app awaits [`Events::next`].
///
/// At most 10,000 events wait to be read. An app that stops reading (a UI in the
/// background, say) does not make the client hold more: newer events are dropped and one
/// [`Event::EventsLost`] takes their place, after which the app reloads what it shows from
/// the client's local data. Nothing is lost from the store.
pub struct Events {
    receiver: mpsc::Receiver<Event>,
}

impl Events {
    /// The next event; `None` once the client is gone.
    pub async fn next(&mut self) -> Option<Event> {
        self.receiver.recv().await
    }

    /// The next event if one is waiting.
    pub fn try_next(&mut self) -> Option<Event> {
        self.receiver.try_recv().ok()
    }
}

/// How a sign-in proves who the user is.
enum Proof {
    Password {
        email: String,
        password: String,
    },
    /// An Entra ID access token the app obtained (MSAL); `handle` names a new account.
    Entra {
        access_token: String,
        handle: Option<String>,
    },
}

impl Proof {
    /// New tokens for `device`, a device this client registered before.
    async fn device(&self, auth: &AuthApi, device: DeviceId) -> Result<Tokens, SdkError> {
        match self {
            Self::Password { email, password } => auth.login_device(email, password, device).await,
            Self::Entra {
                access_token,
                handle,
            } => {
                auth.entra_device(access_token, handle.as_deref(), device)
                    .await
            }
        }
    }

    /// A registration token for a new device.
    async fn registration(&self, auth: &AuthApi) -> Result<String, SdkError> {
        match self {
            Self::Password { email, password } => auth.login(email, password).await,
            Self::Entra {
                access_token,
                handle,
            } => auth.entra(access_token, handle.as_deref()).await,
        }
    }
}

/// A push-messenger client.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: Option<Runtime>,
    shared: Arc<Shared>,
    credentials: Arc<Credentials>,
    auth: AuthApi,
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<ConnectionState>,
    state_sender: Arc<watch::Sender<ConnectionState>>,
    request_timeout: Duration,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Never blocks, so the last clone may be dropped inside an async context.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("state", &*self.inner.state.borrow())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Opens the local store and starts the client's runtime. It does not connect until
    /// [`Client::connect`]; once connected, whatever the outbox held is sent.
    /// [`SdkError::WrongKey`] if `config.database_key` does not open the database.
    pub fn open(config: Config, secrets: Arc<dyn SecretStore>) -> Result<(Self, Events), SdkError> {
        config.check_transport()?;
        let auth = AuthApi::new(&config.auth_url, config.host.as_deref())?;
        let credentials = Arc::new(Credentials::new(secrets, auth.clone()));
        let mut store = Store::open(&config.database_path, config.database_key.as_ref())?;
        if let Some(session) = credentials.session()? {
            store.bind_owner(session.user_id)?;
        }
        // Last: a runtime dropped by an early return could not be dropped in async code.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("push-sdk")
            .enable_all()
            .build()
            .map_err(|error| SdkError::Storage(format!("cannot start the runtime: {error}")))?;
        let (event_sender, receiver) = mpsc::channel(EVENT_QUEUE);
        let shared = Arc::new(Shared::new(store, event_sender));
        let (state_sender, state) = watch::channel(ConnectionState::Offline);
        let state_sender = Arc::new(state_sender);
        let (commands, command_receiver) = mpsc::unbounded_channel();
        let endpoint = Endpoint {
            socket_url: config.socket_url,
            host: config.host,
        };
        let connection = Connection::new(
            shared.clone(),
            credentials.clone(),
            endpoint,
            state_sender.clone(),
            command_receiver,
        );
        runtime.spawn(connection.run());
        let inner = Inner {
            runtime: Some(runtime),
            shared,
            credentials,
            auth,
            commands,
            state,
            state_sender,
            request_timeout: config.request_timeout,
        };
        Ok((
            Self {
                inner: Arc::new(inner),
            },
            Events { receiver },
        ))
    }

    /// Runs `future` on the client's runtime. The work is aborted when the returned future
    /// is dropped (a binding that gives up on a call, or a `close` that cancels it), so
    /// it never outlives its caller holding a clone of the client. What must not be cut
    /// short runs through [`shielded`].
    async fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, SdkError>> + Send + 'static,
    ) -> Result<T, SdkError> {
        let runtime = self.inner.runtime.as_ref().ok_or(SdkError::Closed)?;
        let task = runtime.spawn(future);
        let _abort = AbortOnDrop(task.abort_handle());
        task.await.unwrap_or(Err(SdkError::Closed))
    }

    /// Starts `future` on the client's runtime and returns at once: for the bindings,
    /// which settle a promise or call back when it ends, so they need no runtime of their
    /// own (spec §8.5). A future still running when the client closes is dropped
    /// unfinished. The returned handle aborts it early (a binding aborts its pending calls
    /// when the app closes the client). [`SdkError::Closed`] once the client is closed.
    pub fn spawn(
        &self,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<tokio::task::AbortHandle, SdkError> {
        let runtime = self.inner.runtime.as_ref().ok_or(SdkError::Closed)?;
        Ok(runtime.spawn(future).abort_handle())
    }

    fn command(&self, command: Command) -> Result<(), SdkError> {
        self.inner
            .commands
            .send(command)
            .map_err(|_| SdkError::Closed)
    }

    // ----- account -----

    /// Signs up; auth-svc emails a verification link. Answered the same whether or not
    /// the address is known.
    pub async fn sign_up(
        &self,
        email: &str,
        password: &str,
        handle: &str,
        display_name: &str,
    ) -> Result<(), SdkError> {
        let auth = self.inner.auth.clone();
        let (email, password, handle, display_name) = (
            email.to_owned(),
            password.to_owned(),
            handle.to_owned(),
            display_name.to_owned(),
        );
        self.run(async move {
            auth.sign_up(&email, &password, &handle, &display_name)
                .await
        })
        .await
    }

    /// Sends the verification email again.
    pub async fn resend_verification(&self, email: &str) -> Result<(), SdkError> {
        let (auth, email) = (self.inner.auth.clone(), email.to_owned());
        self.run(async move { auth.resend_verification(&email).await })
            .await
    }

    /// Emails a password-reset link. Resetting signs every device out.
    pub async fn forgot_password(&self, email: &str) -> Result<(), SdkError> {
        let (auth, email) = (self.inner.auth.clone(), email.to_owned());
        self.run(async move { auth.forgot_password(&email).await })
            .await
    }

    /// Signs in on this device (spec §5.3). The device this client registered before is
    /// signed in again if it is still active; otherwise a device is registered, with a
    /// `registration_id` kept until the registration is answered and its tokens saved, so
    /// a retry, even after a restart, never creates a second device. Then call
    /// [`Client::connect`]. A connection that is up (or coming up) is stopped first, and
    /// this waits for it, so one for an earlier user never writes into the new user's
    /// data ([`SdkError::ConnectionBusy`] if it does not stop in time: the new session is
    /// saved then, and signing in again completes the sign-in). Connect again afterwards.
    pub async fn sign_in(
        &self,
        email: &str,
        password: &str,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let proof = Proof::Password {
            email: email.to_owned(),
            password: password.to_owned(),
        };
        self.sign_in_with(proof, device_name, platform).await
    }

    /// Signs in on this device with a Microsoft Entra ID access token for auth-svc's API,
    /// which the app obtained itself with MSAL (scope `api://<api-client-id>/access_as_user`,
    /// docs/entra-sign-in.md). `handle` names the account on its first sign-in only. The
    /// device is kept exactly as by [`Client::sign_in`]. After the session's maximum age
    /// the SDK reports `SignedOut`: get a new token and call this again (same device). The
    /// token is sent once and never stored or logged.
    pub async fn sign_in_with_entra(
        &self,
        access_token: &str,
        handle: Option<&str>,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let proof = Proof::Entra {
            access_token: access_token.to_owned(),
            handle: handle.map(str::to_owned),
        };
        self.sign_in_with(proof, device_name, platform).await
    }

    async fn sign_in_with(
        &self,
        proof: Proof,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let inner = self.inner.clone();
        let device_name = device_name.to_owned();
        self.run(async move {
            let credentials = &inner.credentials;
            let mut tokens = None;
            if let Some(device) = credentials.last_device()? {
                match proof.device(&inner.auth, device).await {
                    Ok(found) => tokens = Some(found),
                    // Revoked meanwhile: register anew.
                    Err(SdkError::Server { code, .. }) if code == "device_not_found" => {
                        credentials.forget_device()?;
                    }
                    Err(error) => return Err(error),
                }
            }
            let tokens = match tokens {
                Some(tokens) => tokens,
                None => {
                    let registration = proof.registration(&inner.auth).await?;
                    let register = |registration_id| {
                        inner.auth.register_device(
                            &registration,
                            registration_id,
                            &device_name,
                            platform,
                        )
                    };
                    match register(credentials.registration_id()?).await {
                        // The id belongs to a device revoked since: it stays spent, so the
                        // registration starts again with a new one.
                        Err(SdkError::Server { code, .. }) if code == "device_not_found" => {
                            credentials.registration_done()?;
                            register(credentials.registration_id()?).await?
                        }
                        answer => answer?,
                    }
                }
            };
            // Saved before a registration_id is forgotten: if saving fails, a retry finds
            // the same device again.
            let session = credentials.sign_in(tokens).await?;
            credentials.registration_done()?;
            let user = session.user_id;
            // A connection of the earlier user may be live (or handshaking): it stops
            // before the store changes hands, and it does not come back as the earlier
            // user, the session being the new one's now. The app connects again. Not
            // cut short once the session is saved: a store left with the earlier user's
            // data under the new session would let a connection write into it.
            let tail = inner.clone();
            shielded(async move {
                stop_connection(&tail).await?;
                blocking(&tail.shared, move |store| store.bind_owner(user)).await?;
                Ok(())
            })
            .await?;
            Ok(Account {
                user_id: session.user_id,
                device_id: session.device_id,
            })
        })
        .await
    }

    /// The signed-in user and device, if any.
    pub fn account(&self) -> Result<Option<Account>, SdkError> {
        Ok(self.inner.credentials.session()?.map(|session| Account {
            user_id: session.user_id,
            device_id: session.device_id,
        }))
    }

    /// Signs this device out for good: the credentials are forgotten first, then the
    /// client disconnects, erases the local store, outbox included, and asks auth-svc to
    /// revoke the device. The local part always happens, also offline (an app that
    /// ends meanwhile has still signed out), and a failed keychain delete does not stop
    /// the erase or the revocation: the first error is returned. An error may say that
    /// auth-svc did not confirm
    /// the revocation, and the device stays in the account's device list until it is
    /// removed from another device ([`Client::revoke_device`]).
    /// [`SdkError::ConnectionBusy`]: the connection did not stop in time, so the local
    /// data was not erased; call again later.
    pub async fn sign_out(&self) -> Result<(), SdkError> {
        let inner = self.inner.clone();
        self.run(async move {
            // The session goes first, so no connection can start again; then the one that
            // runs is stopped, and only then is the store erased: nothing of it can come
            // after the erase. An explicit sign-out always erases and always tries to
            // revoke: a failed keychain delete does not keep either, and the first error
            // is reported. If the stop is not confirmed the store is left alone. The local
            // part is not cut short (the session is gone then, and a store left unerased
            // would keep the user's data); only the revocation, which an app that ends
            // meanwhile loses just the same, is abandoned when the call is dropped.
            let local = inner.clone();
            let (session, local_error) = shielded(async move {
                let (session, forgotten) = local.credentials.sign_out_for_good().await;
                let stopped = stop_connection(&local).await.err();
                let erased = if stopped.is_none() {
                    blocking(&local.shared, |store| store.erase_all())
                        .await
                        .err()
                } else {
                    None
                };
                Ok((session, forgotten.or(stopped).or(erased)))
            })
            .await?;
            let revoked = match session {
                Some(session) => revoke(&inner, session).await.err(),
                None => None,
            };
            match local_error.or(revoked) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
        .await
    }

    // ----- connection -----

    /// Connects, and keeps reconnecting until [`Client::disconnect`], a sign-out (4001),
    /// the device's removal (4003) or a takeover by a copy opened elsewhere (4007). The
    /// state is `Connecting` when this returns, so a request made right after waits for
    /// the connection instead of failing as offline.
    pub fn connect(&self) -> Result<(), SdkError> {
        let state = *self.inner.state.borrow();
        if matches!(
            state,
            ConnectionState::Offline
                | ConnectionState::SignedOut
                | ConnectionState::DeviceRemoved
                | ConnectionState::OpenedElsewhere
        ) {
            set_state(
                &self.inner.state_sender,
                &self.inner.shared,
                ConnectionState::Connecting,
            );
        }
        self.command(Command::Connect)
    }

    /// Closes the connection and stops reconnecting. The outbox waits.
    pub fn disconnect(&self) -> Result<(), SdkError> {
        self.command(Command::Disconnect)
    }

    /// Shuts the client down and waits until the connection has stopped, the runtime has
    /// ended and the local database is closed, so the file can be opened again, rekeyed
    /// or deleted at once. Dropping the client does the same without waiting (it never
    /// blocks); an app that does not need to wait may simply drop it.
    ///
    /// Only the last handle can close the database. Requests running on other clones are
    /// aborted when their callers drop them; with other clones still alive this waits up
    /// to 5 s for them to go, and then (the connection is stopped, the database still
    /// open until they drop) returns [`SdkError::Timeout`].
    pub async fn close(self) -> Result<(), SdkError> {
        let inner = self.inner.clone();
        let stopped = self.run(async move { stop_connection(&inner).await }).await;
        // The caller may have no tokio runtime (the bindings' threads), so the waiting
        // sleeps on the client's.
        let handle = self
            .inner
            .runtime
            .as_ref()
            .ok_or(SdkError::Closed)?
            .handle()
            .clone();
        let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
        let mut shared = self.inner;
        let mut inner = loop {
            match Arc::try_unwrap(shared) {
                Ok(inner) => break inner,
                Err(still_shared) if tokio::time::Instant::now() >= deadline => {
                    drop(still_shared);
                    return Err(SdkError::Timeout);
                }
                Err(still_shared) => {
                    shared = still_shared;
                    let _ = handle
                        .spawn(async { tokio::time::sleep(Duration::from_millis(10)).await })
                        .await;
                }
            }
        };
        let runtime = inner.runtime.take();
        let (done, finished) = oneshot::channel();
        // Dropping a runtime waits for its tasks, which an async caller must not do.
        std::thread::spawn(move || {
            if let Some(runtime) = runtime {
                runtime.shutdown_timeout(STOP_TIMEOUT);
            }
            drop(inner);
            let _ = done.send(());
        });
        let _ = finished.await;
        stopped
    }

    pub fn connection_state(&self) -> ConnectionState {
        *self.inner.state.borrow()
    }

    /// Encrypts the local database with `key`, re-encrypts it with a new one (the user
    /// changed their PIN), or with `None` decrypts it. The next [`Client::open`] needs the
    /// new key in [`Config::database_key`]; the client does not keep it.
    pub async fn change_database_key(&self, key: Option<&DatabaseKey>) -> Result<(), SdkError> {
        let (shared, key) = (self.inner.shared.clone(), key.cloned());
        self.run(
            async move { blocking(&shared, move |store| store.change_key(key.as_ref())).await },
        )
        .await
    }

    /// Waits until the client is connected, up to `wait`.
    pub async fn wait_connected(&self, wait: Duration) -> Result<(), SdkError> {
        let mut state = self.inner.state.clone();
        self.run(async move {
            match tokio::time::timeout(
                wait,
                state.wait_for(|state| *state == ConnectionState::Connected),
            )
            .await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(_)) => Err(SdkError::Closed),
                Err(_) => Err(SdkError::Offline),
            }
        })
        .await
    }

    // ----- local data -----

    /// Every conversation this device knows, most recently changed first.
    pub fn conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        self.inner.shared.store().conversations()
    }

    pub fn conversation(&self, id: ConversationId) -> Result<Option<Conversation>, SdkError> {
        self.inner.shared.store().conversation(id)
    }

    /// Up to `limit` stored messages before `before_seq`, oldest first; without
    /// `before_seq`, the newest ones followed by those still in the outbox. Older ones
    /// come with [`Client::fetch_history`].
    pub fn messages(
        &self,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        self.inner
            .shared
            .store()
            .messages(conversation, before_seq, limit)
    }

    /// The user's relationships as this device knows them.
    pub fn contacts(&self) -> Result<Vec<Contact>, SdkError> {
        self.inner.shared.store().contacts()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use push_proto::v1::Ready;
    use push_proto::v1::frame::Body;

    use uuid::Uuid;

    use super::*;
    use crate::MemorySecretStore;
    use crate::secrets::REGISTRATION_KEY;
    use crate::store::bytes;
    use crate::testing::{FakeAuth, FakeGateway, recv, send};

    #[test]
    fn plaintext_urls_are_refused_beyond_this_machine_unless_allowed() {
        let open = |auth: &str, socket: &str, allow: bool| {
            let config = Config {
                allow_insecure_transport: allow,
                ..Config::new(auth, socket, ":memory:")
            };
            Client::open(config, Arc::new(MemorySecretStore::new())).map(|_| ())
        };
        let refused = |result: Result<(), SdkError>| matches!(result, Err(SdkError::Invalid(_)));
        assert!(
            open(
                "https://push.example",
                "wss://push.example/v1/socket",
                false
            )
            .is_ok()
        );
        for (auth, socket) in [
            ("http://127.0.0.1:8080", "ws://127.0.0.1:8080/v1/socket"),
            ("http://[::1]:8080", "ws://localhost/v1/socket"),
            ("http://push.localhost", "ws://push.test/v1/socket"),
            ("http://minikube.local", "ws://minikube.local/v1/socket"),
        ] {
            assert!(open(auth, socket, false).is_ok(), "{auth} {socket}");
        }
        assert!(refused(open(
            "http://push.example",
            "wss://push.example/v1/socket",
            false
        )));
        assert!(refused(open(
            "https://push.example",
            "ws://192.168.49.2/v1/socket",
            false
        )));
        assert!(refused(open(
            "ftp://push.example",
            "wss://push.example/v1/socket",
            true
        )));
        assert!(refused(open(
            "not a url",
            "wss://push.example/v1/socket",
            false
        )));
        assert!(
            open(
                "http://push-auth:8080",
                "ws://push-gateway:8080/v1/socket",
                true
            )
            .is_ok()
        );
    }

    pub(super) struct Fixture {
        pub client: Client,
        pub events: Events,
        pub auth: FakeAuth,
        pub gateway: FakeGateway,
        pub secrets: Arc<MemorySecretStore>,
        pub database: String,
    }

    impl Fixture {
        /// A client signed in to fake services, not connected, on its own database.
        pub async fn new() -> Self {
            let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
            let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
            let secrets = Arc::new(MemorySecretStore::new());
            let (client, events) = open(&auth, &gateway, &database, &secrets);
            client
                .sign_in("a@example.com", "password", "phone", Platform::Ios)
                .await
                .unwrap();
            Self {
                client,
                events,
                auth,
                gateway,
                secrets,
                database,
            }
        }

        pub fn events(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            while let Some(event) = self.events.try_next() {
                events.push(event);
            }
            events
        }

        /// A direct conversation with someone, known locally.
        pub fn conversation(&self) -> (ConversationId, UserId) {
            let (id, peer) = (ConversationId::new(), UserId::new());
            self.client
                .inner
                .shared
                .store()
                .open_direct(id, peer)
                .unwrap();
            (id, peer)
        }

        /// Accepts the connection and answers `Identify` with `Ready`.
        pub async fn ready(&mut self) -> crate::testing::Server {
            let mut socket = self.gateway.next().await;
            let Body::Identify(_) = recv(&mut socket).await else {
                panic!("Identify first");
            };
            let ready = Ready {
                user_id: bytes(self.auth.user.0),
                device_id: bytes(self.auth.device.0),
                heartbeat_ms: 30_000,
            };
            send(&mut socket, Body::Ready(ready)).await;
            socket
        }
    }

    /// Queues a message straight in the store (sending comes with `client/sending.rs`).
    fn queue(client: &Client, conversation: ConversationId, text: &str) {
        client
            .inner
            .shared
            .store()
            .queue_send(conversation, push_domain::ClientMsgId::new(), text, 1)
            .unwrap();
    }

    pub(super) fn open(
        auth: &FakeAuth,
        gateway: &FakeGateway,
        database: &str,
        secrets: &Arc<MemorySecretStore>,
    ) -> (Client, Events) {
        let config = Config {
            request_timeout: Duration::from_secs(3),
            ..Config::new(&auth.url, &gateway.url, database)
        };
        Client::open(config, secrets.clone()).unwrap()
    }

    #[tokio::test]
    async fn signing_in_registers_once_and_finds_the_same_device_again() {
        let f = Fixture::new().await;
        assert_eq!(f.auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(f.secrets.load(REGISTRATION_KEY).unwrap(), None);
        let account = f.client.account().unwrap().unwrap();
        assert_eq!(
            (account.user_id, account.device_id),
            (f.auth.user, f.auth.device)
        );

        // Signed out by the server (a password reset): the same device signs in again.
        f.client.inner.credentials.sign_out().await.unwrap();
        f.client
            .sign_in("a@example.com", "new password", "phone", Platform::Ios)
            .await
            .unwrap();
        assert_eq!(f.auth.device_logins.load(Ordering::SeqCst), 1);
        assert_eq!(f.auth.registrations.load(Ordering::SeqCst), 1);

        // Signing out for good revokes the device and empties the store.
        let (conversation, _) = f.conversation();
        queue(&f.client, conversation, "unsent");
        f.client.sign_out().await.unwrap();
        assert_eq!(f.auth.revocations.load(Ordering::SeqCst), 1);
        assert_eq!(f.client.account().unwrap(), None);
        assert!(f.client.conversations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn signing_out_offline_still_erases_and_reports_the_failed_revocation() {
        let f = Fixture::new().await;
        let (conversation, _) = f.conversation();
        queue(&f.client, conversation, "unsent");
        f.auth.fail_revocations.store(true, Ordering::SeqCst);
        let error = f.client.sign_out().await.unwrap_err();
        assert_eq!(error.code(), "http_500");
        assert_eq!(f.client.account().unwrap(), None);
        assert!(f.client.conversations().unwrap().is_empty());
        assert!(f.client.inner.shared.store().outbox().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_spent_registration_id_is_replaced() {
        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(MemorySecretStore::new());
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let (client, _events) = open(&auth, &gateway, &database, &secrets);
        auth.spend_next_registration.store(true, Ordering::SeqCst);
        client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        let ids = auth.registration_ids.lock().unwrap().clone();
        assert_eq!(ids.len(), 2);
        assert_ne!(
            ids[0], ids[1],
            "a new registration_id after device_not_found"
        );
        assert_eq!(secrets.load(REGISTRATION_KEY).unwrap(), None);
    }

    /// A secret store whose next save of the session fails, as a locked keychain might.
    #[derive(Default)]
    struct LockedOnce {
        values: MemorySecretStore,
        locked: std::sync::atomic::AtomicBool,
    }

    impl SecretStore for LockedOnce {
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, crate::SecretError> {
            self.values.load(key)
        }

        fn save(&self, key: &str, value: &[u8]) -> Result<(), crate::SecretError> {
            if key == crate::secrets::SESSION_KEY && self.locked.swap(false, Ordering::SeqCst) {
                return Err(crate::SecretError("the keychain is locked".to_owned()));
            }
            self.values.save(key, value)
        }

        fn delete(&self, key: &str) -> Result<(), crate::SecretError> {
            self.values.delete(key)
        }
    }

    #[tokio::test]
    async fn the_registration_id_is_kept_until_the_session_is_saved() {
        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(LockedOnce::default());
        secrets.locked.store(true, Ordering::SeqCst);
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let config = Config::new(&auth.url, &gateway.url, &database);
        let (client, _events) = Client::open(config, secrets.clone()).unwrap();
        let failed = client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await;
        assert!(matches!(failed, Err(SdkError::Secrets(_))));
        assert!(
            secrets.load(REGISTRATION_KEY).unwrap().is_some(),
            "a retry must name the same registration"
        );
        let account = client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        assert_eq!(account.device_id, auth.device);
        assert_eq!(auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(secrets.load(REGISTRATION_KEY).unwrap(), None);
    }

    /// Streams deliveries for `conversation` until the connection is gone. It never reads,
    /// so a close handshake from the client takes its whole wait.
    fn stream_deliveries(
        mut socket: crate::testing::Server,
        conversation: ConversationId,
        peer: UserId,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            for seq in 1.. {
                let entry = crate::store::tests::entry(
                    conversation,
                    peer,
                    seq,
                    push_proto::v1::EntryKind::Message,
                );
                if !crate::testing::try_send(&mut socket, Body::Deliver(entry)).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }

    fn is_empty(client: &Client) -> bool {
        let store = client.inner.shared.store();
        store.owner().unwrap().is_none()
            && store.conversations().unwrap().is_empty()
            && store.outbox().unwrap().is_empty()
    }

    #[tokio::test]
    async fn signing_out_waits_for_the_connection_and_nothing_comes_back() {
        let mut f = Fixture::new().await;
        let (conversation, peer) = f.conversation();
        queue(&f.client, conversation, "unsent");
        f.client.connect().unwrap();
        let socket = f.ready().await;
        f.client
            .wait_connected(Duration::from_secs(5))
            .await
            .unwrap();
        let _deliveries = stream_deliveries(socket, conversation, peer);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!is_empty(&f.client), "deliveries are landing");

        f.client.sign_out().await.unwrap();
        assert_eq!(f.client.connection_state(), ConnectionState::Offline);
        assert!(is_empty(&f.client));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(is_empty(&f.client));
    }

    #[tokio::test]
    async fn signing_out_during_the_handshake_cannot_rebind_the_wiped_store() {
        let f = Fixture::new().await;
        let session = f.client.account().unwrap().unwrap();
        // Expiring: the connection refreshes (slowly) before it binds the store.
        crate::secrets::Session {
            user_id: session.user_id,
            device_id: session.device_id,
            access_token: "access-old".to_owned(),
            access_expires_at: crate::credentials::unix_now() + 10,
            refresh_token: "refresh-old".to_owned(),
        }
        .save(f.secrets.as_ref())
        .unwrap();
        f.auth.refresh_delay_ms.store(400, Ordering::SeqCst);
        f.client.connect().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        f.client.sign_out().await.unwrap();
        assert!(is_empty(&f.client));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(is_empty(&f.client));
    }

    #[tokio::test]
    async fn signing_in_as_another_user_stops_the_old_connection_first() {
        let mut f = Fixture::new().await;
        let (conversation, peer) = f.conversation();
        f.client.connect().unwrap();
        let socket = f.ready().await;
        f.client
            .wait_connected(Duration::from_secs(5))
            .await
            .unwrap();
        let _deliveries = stream_deliveries(socket, conversation, peer);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(f.client.conversation(conversation).unwrap().is_some());

        let user_b = UserId::new();
        *f.auth.identity.lock().unwrap() = (user_b, DeviceId::new());
        let account = f
            .client
            .sign_in("b@example.com", "password", "tablet", Platform::Ios)
            .await
            .unwrap();
        assert_eq!(account.user_id, user_b);
        assert_eq!(f.client.connection_state(), ConnectionState::Offline);
        let holds_a_data = |client: &Client| {
            let store = client.inner.shared.store();
            store.owner().unwrap() != Some(user_b) || !store.conversations().unwrap().is_empty()
        };
        assert!(!holds_a_data(&f.client));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!holds_a_data(&f.client));
    }

    /// A keychain that cannot delete the session.
    #[derive(Default)]
    struct Undeletable(MemorySecretStore);

    impl SecretStore for Undeletable {
        fn load(&self, key: &str) -> Result<Option<Vec<u8>>, crate::SecretError> {
            self.0.load(key)
        }

        fn save(&self, key: &str, value: &[u8]) -> Result<(), crate::SecretError> {
            self.0.save(key, value)
        }

        fn delete(&self, key: &str) -> Result<(), crate::SecretError> {
            if key == crate::secrets::SESSION_KEY {
                return Err(crate::SecretError("the keychain is locked".to_owned()));
            }
            self.0.delete(key)
        }
    }

    #[tokio::test]
    async fn signing_out_erases_and_revokes_even_when_the_keychain_delete_fails() {
        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(Undeletable::default());
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let config = Config::new(&auth.url, &gateway.url, &database);
        let (client, _events) = Client::open(config, secrets).unwrap();
        client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        let conversation = ConversationId::new();
        client
            .inner
            .shared
            .store()
            .open_direct(conversation, UserId::new())
            .unwrap();
        queue(&client, conversation, "unsent");
        let error = client.sign_out().await.unwrap_err();
        assert!(matches!(error, SdkError::Secrets(_)));
        assert!(is_empty(&client));
        assert_eq!(auth.revocations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn spawned_work_runs_on_the_client_runtime() {
        let f = Fixture::new().await;
        let (done, ran) = oneshot::channel();
        f.client
            .spawn(async move {
                let _ = done.send(std::thread::current().name().map(str::to_owned));
            })
            .unwrap();
        assert_eq!(ran.await.unwrap().as_deref(), Some("push-sdk"));
    }

    #[tokio::test]
    async fn spawned_work_can_be_aborted_through_its_handle() {
        let f = Fixture::new().await;
        let (held, dropped) = oneshot::channel::<()>();
        let handle = f
            .client
            .spawn(async move {
                let _held = held;
                std::future::pending::<()>().await;
            })
            .unwrap();
        handle.abort();
        let outcome = tokio::time::timeout(Duration::from_secs(5), dropped)
            .await
            .expect("the aborted future is dropped");
        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn dropping_a_request_aborts_its_work() {
        let f = Fixture::new().await;
        let (held, dropped) = oneshot::channel::<()>();
        let request = f.client.run(async move {
            let _held = held;
            std::future::pending::<Result<(), SdkError>>().await
        });
        let waited = tokio::time::timeout(Duration::from_millis(100), request).await;
        assert!(waited.is_err(), "the request never answers");
        let outcome = tokio::time::timeout(Duration::from_secs(5), dropped)
            .await
            .expect("the work of the dropped request is dropped too");
        assert!(outcome.is_err());
    }

    /// The bindings' tests talk to these doubles from outside Rust.
    #[tokio::test]
    async fn the_automatic_gateway_stores_sends_and_answers_them() {
        let auth = FakeAuth::start().await;
        let gateway = crate::testing::AutoGateway::start(auth.identity.clone()).await;
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let config = Config::new(&auth.url, &gateway.url, &database);
        let (client, mut events) =
            Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
        let wrong = client
            .sign_in(
                "a@example.com",
                crate::testing::WRONG_PASSWORD,
                "phone",
                Platform::Web,
            )
            .await
            .unwrap_err();
        assert_eq!(wrong.code(), "invalid_credentials");
        client
            .sign_in("a@example.com", "password", "phone", Platform::Web)
            .await
            .unwrap();
        client.connect().unwrap();
        let conversation = client.open_direct(UserId::new()).await.unwrap();
        let sent = client.send(conversation.id, "hello").unwrap();
        let (mut stored, mut answered) = (None, None);
        while stored.is_none() || answered.is_none() {
            let event = tokio::time::timeout(Duration::from_secs(10), events.next())
                .await
                .expect("an event within 10 s")
                .unwrap();
            match event {
                Event::MessageSent(message) if message.client_msg_id == sent.client_msg_id => {
                    stored = message.seq;
                }
                Event::MessageAdded(message) if message.sender_id != auth.user => {
                    answered = Some((message.seq, message.body));
                }
                _ => {}
            }
        }
        assert_eq!(stored, Some(1));
        assert_eq!(answered, Some((Some(2), Some("hello".to_owned()))));
        let unknown = client
            .redeem_invite(crate::testing::UNKNOWN_INVITE)
            .await
            .unwrap_err();
        assert_eq!(unknown.code(), "invite_not_found");
        assert_eq!(client.list_contacts().await, Ok(Vec::new()));
        assert_eq!(
            client.revoke_invite("ABCDEFGHJK").await.unwrap_err().code(),
            "rejected"
        );
    }

    #[tokio::test]
    async fn an_encrypted_database_opens_only_with_its_key() {
        let scratch = crate::store::tests::Scratch::new();
        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(MemorySecretStore::new());
        let salt = DatabaseKey::new_salt();
        let key = DatabaseKey::derive(b"2468", &salt).unwrap();
        let config = |key: Option<&DatabaseKey>| Config {
            database_key: key.cloned(),
            ..Config::new(&auth.url, &gateway.url, &scratch.database())
        };
        let (client, events) = Client::open(config(Some(&key)), secrets.clone()).unwrap();
        client
            .sign_in("a@example.com", "password", "phone", Platform::Ios)
            .await
            .unwrap();
        let conversation = ConversationId::new();
        client
            .inner
            .shared
            .store()
            .open_direct(conversation, UserId::new())
            .unwrap();
        queue(&client, conversation, "for the key holder");
        client.close().await.unwrap();
        drop(events);
        assert!(!scratch.holds("for the key holder"));

        for wrong in [None, Some(DatabaseKey::derive(b"1357", &salt).unwrap())] {
            let opened = Client::open(config(wrong.as_ref()), secrets.clone());
            assert_eq!(opened.err(), Some(SdkError::WrongKey));
        }
        let (client, _events) = Client::open(config(Some(&key)), secrets.clone()).unwrap();
        assert_eq!(client.messages(conversation, None, 10).unwrap().len(), 1);
        let changed = DatabaseKey::derive(b"new pin", &salt).unwrap();
        client.change_database_key(Some(&changed)).await.unwrap();
        client.close().await.unwrap();
        let (client, _events) = Client::open(config(Some(&changed)), secrets).unwrap();
        assert_eq!(client.messages(conversation, None, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_entra_sign_in_registers_once_and_signs_the_same_device_in_again() {
        use serde_json::json;

        use crate::testing::WRONG_ENTRA_TOKEN;

        let (auth, gateway) = (FakeAuth::start().await, FakeGateway::start().await);
        let secrets = Arc::new(MemorySecretStore::new());
        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
        let (client, _events) = open(&auth, &gateway, &database, &secrets);
        let account = client
            .sign_in_with_entra("entra-access-token", Some("erin"), "laptop", Platform::Web)
            .await
            .unwrap();
        assert_eq!(
            (account.user_id, account.device_id),
            (auth.user, auth.device)
        );
        assert_eq!(auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(secrets.load(REGISTRATION_KEY).unwrap(), None);

        // Signed out by the server (the session's maximum age): the same device again.
        client.inner.credentials.sign_out().await.unwrap();
        client
            .sign_in_with_entra("a-newer-entra-token", None, "laptop", Platform::Web)
            .await
            .unwrap();
        assert_eq!(auth.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(
            auth.entra_bodies.lock().unwrap().clone(),
            vec![
                json!({"token": "entra-access-token", "handle": "erin"}),
                json!({"token": "a-newer-entra-token", "handle": null, "device_id": auth.device}),
            ]
        );
        let refused = client
            .sign_in_with_entra(WRONG_ENTRA_TOKEN, None, "laptop", Platform::Web)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), "entra_token_invalid");
    }

    #[tokio::test]
    async fn the_profile_changes_its_handle_or_display_name() {
        use crate::testing::TAKEN_HANDLE;

        let f = Fixture::new().await;
        let renamed = f.client.update_profile(None, Some("erin")).await.unwrap();
        assert_eq!(
            (
                renamed.user_id,
                renamed.handle.as_str(),
                renamed.display_name.as_str()
            ),
            (f.auth.user, "erin", "Alice")
        );
        let named = f
            .client
            .update_profile(Some("Erin E."), None)
            .await
            .unwrap();
        assert_eq!(named.display_name, "Erin E.");
        let taken = f
            .client
            .update_profile(None, Some(TAKEN_HANDLE))
            .await
            .unwrap_err();
        assert_eq!(taken.code(), "handle_taken");
        assert_eq!(
            f.client
                .update_profile(None, None)
                .await
                .unwrap_err()
                .code(),
            "invalid"
        );
    }
}
