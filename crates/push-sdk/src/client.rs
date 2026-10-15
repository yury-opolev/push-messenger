//! The SDK's public face (spec §8.2): one [`Client`] per signed-in app profile, and the
//! [`Events`] stream it reports on.
//!
//! The client owns its own tokio runtime (spec §8.5), so it works the same from an async
//! Rust app, a Node binding or a C binding: its async methods run on the client's runtime
//! and can be awaited from any executor; its plain methods read or change the local store
//! at once. It is cheap to clone, and every clone is the same client.

mod sending;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use push_domain::{ConversationId, DeviceId, UserId};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};

use crate::SdkError;
use crate::auth::{AuthApi, Platform};
use crate::connection::{Command, Connection, Endpoint, set_state};
use crate::credentials::{Credentials, MIN_ACCESS_LIFETIME_S, is_refused, unix_now};
use crate::engine::{EVENT_QUEUE, Shared};
use crate::key::DatabaseKey;
use crate::model::{ConnectionState, Contact, Conversation, Event, Message};
use crate::secrets::SecretStore;
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
        }
    }
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

    /// Runs `future` on the client's runtime.
    async fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T, SdkError>> + Send + 'static,
    ) -> Result<T, SdkError> {
        let runtime = self.inner.runtime.as_ref().ok_or(SdkError::Closed)?;
        runtime.spawn(future).await.unwrap_or(Err(SdkError::Closed))
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
    /// [`Client::connect`].
    pub async fn sign_in(
        &self,
        email: &str,
        password: &str,
        device_name: &str,
        platform: Platform,
    ) -> Result<Account, SdkError> {
        let inner = self.inner.clone();
        let (email, password, device_name) = (
            email.to_owned(),
            password.to_owned(),
            device_name.to_owned(),
        );
        self.run(async move {
            let credentials = &inner.credentials;
            let mut tokens = None;
            if let Some(device) = credentials.last_device()? {
                match inner.auth.login_device(&email, &password, device).await {
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
                    let registration = inner.auth.login(&email, &password).await?;
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
            let session = credentials.sign_in(tokens)?;
            credentials.registration_done()?;
            let user = session.user_id;
            blocking(&inner.shared, move |store| store.bind_owner(user)).await?;
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

    /// Signs this device out for good: the client disconnects, forgets the credentials,
    /// erases the local store, outbox included, and then asks auth-svc to revoke the
    /// device. The local part comes first and always happens, also offline (an app that
    /// ends meanwhile has still signed out); an error says that auth-svc did not confirm
    /// the revocation, and the device stays in the account's device list until it is
    /// removed from another device ([`Client::revoke_device`]).
    pub async fn sign_out(&self) -> Result<(), SdkError> {
        let inner = self.inner.clone();
        self.command(Command::Disconnect)?;
        self.run(async move {
            let session = inner.credentials.sign_out_for_good().await?;
            blocking(&inner.shared, |store| store.erase_all()).await?;
            let Some(session) = session else {
                return Ok(());
            };
            // The tokens are no longer stored: an access token about to expire is
            // refreshed in memory only, for this one call.
            let access_token = if session.access_expires_at - unix_now() >= MIN_ACCESS_LIFETIME_S {
                session.access_token
            } else {
                match inner.auth.refresh(&session.refresh_token).await {
                    Ok(tokens) => tokens.access_token,
                    // Refused: the device's tokens are dead already.
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
        drop((client, events));
        assert!(!scratch.holds("for the key holder"));

        for wrong in [None, Some(DatabaseKey::derive(b"1357", &salt).unwrap())] {
            let opened = Client::open(config(wrong.as_ref()), secrets.clone());
            assert_eq!(opened.err(), Some(SdkError::WrongKey));
        }
        let (client, _events) = Client::open(config(Some(&key)), secrets.clone()).unwrap();
        assert_eq!(client.messages(conversation, None, 10).unwrap().len(), 1);
        let changed = DatabaseKey::derive(b"new pin", &salt).unwrap();
        client.change_database_key(Some(&changed)).await.unwrap();
        drop(client);
        let (client, _events) = Client::open(config(Some(&changed)), secrets).unwrap();
        assert_eq!(client.messages(conversation, None, 10).unwrap().len(), 1);
    }
}
