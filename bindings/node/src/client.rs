//! The client, as a class JavaScript holds: `index.js` wraps it.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use napi::bindgen_prelude::{Buffer, Function, Object};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Env, Status};
use napi_derive::napi;
use push_sdk::{
    Client, ClientMsgId, ConversationId, DatabaseKey, DeviceId, Events, SdkError, UserId,
};
use push_sdk_ffi::{block_on, json};
use serde_json::Value;

use crate::pending::{Pending, Tracked};
use crate::promise::{Settle, failure};

/// How many events may wait for the JavaScript thread; past that the events thread waits,
/// and the SDK's own queue (10,000, then `eventsLost`) takes over.
const EVENTS_IN_FLIGHT: usize = 1000;

/// How long close waits for aborted calls to drop their clones of the client.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

type Listener = ThreadsafeFunction<Value, (), Value, Status, false, false, EVENTS_IN_FLIGHT>;

#[napi]
pub struct NativeClient {
    client: Mutex<Option<Client>>,
    events: Mutex<Option<Events>>,
    pending: Arc<Pending>,
}

fn invalid(message: &str) -> napi::Error {
    failure(&SdkError::Invalid(message.to_owned()))
}

fn conversation_id(text: &str) -> Result<ConversationId, SdkError> {
    json::id(text, "conversationId")
}

fn user_id(text: &str) -> Result<UserId, SdkError> {
    json::id(text, "userId")
}

impl NativeClient {
    pub fn new((client, events): (Client, Events)) -> Self {
        Self {
            client: Mutex::new(Some(client)),
            events: Mutex::new(Some(events)),
            pending: Arc::default(),
        }
    }

    fn client(&self) -> napi::Result<Client> {
        self.client
            .lock()
            .ok()
            .and_then(|client| client.clone())
            .ok_or_else(|| failure(&SdkError::Closed))
    }

    /// Runs `work` on the client's runtime; its result settles the promise.
    fn promise<'env, F>(
        &self,
        env: &'env Env,
        work: impl FnOnce(Client) -> F,
    ) -> napi::Result<Object<'env>>
    where
        F: Future<Output = Result<Value, SdkError>> + Send + 'static,
    {
        let client = self.client()?;
        let registration = self
            .pending
            .register()
            .ok_or_else(|| failure(&SdkError::Closed))?;
        let id = registration.id();
        let (settle, promise) = Settle::new(env)?;
        let future = work(client.clone());
        // A task dropped unfinished (aborted by `close`, or never started) drops `settle`,
        // which rejects with `closed`.
        let task = Tracked::new(async move { settle.finish(future.await) }, registration);
        if let Ok(handle) = client.spawn(task) {
            self.pending.started(id, handle);
        }
        Ok(promise)
    }

    /// A call answered at once.
    fn now<T>(&self, work: impl FnOnce(&Client) -> Result<T, SdkError>) -> napi::Result<T> {
        work(&self.client()?).map_err(|error| failure(&error))
    }
}

#[napi]
impl NativeClient {
    /// Delivers every event, as JSON, to `listener` on the JavaScript thread, in order.
    /// Only one listener per client. While it listens, Node keeps running until `close`.
    #[napi(catch_unwind)]
    pub fn on_event(&self, listener: Function<'_, Value, ()>) -> napi::Result<()> {
        let mut events = self
            .events
            .lock()
            .ok()
            .and_then(|mut events| events.take())
            .ok_or_else(|| invalid("the events have a listener already"))?;
        let listener: Listener = listener
            .build_threadsafe_function::<Value>()
            .callee_handled::<false>()
            .max_queue_size::<EVENTS_IN_FLIGHT>()
            .build_callback(|context| Ok(context.value))?;
        std::thread::Builder::new()
            .name("push-sdk-events".to_owned())
            .spawn(move || {
                // Ends when the client is gone; dropping the listener lets Node exit.
                while let Some(event) = block_on(events.next()) {
                    let status =
                        listener.call(json::event(&event), ThreadsafeFunctionCallMode::Blocking);
                    if status != Status::Ok {
                        break;
                    }
                }
            })
            .map_err(|_| invalid("cannot start the events thread"))?;
        Ok(())
    }

    /// Stops the connection, closes the database and ends the client's threads; later calls
    /// fail with `closed`.
    #[napi(catch_unwind)]
    pub fn close<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        let client = self.client.lock().ok().and_then(|mut client| client.take());
        // Pending calls reject with `closed`; the clones their tasks hold must be gone
        // before the database can close.
        self.pending.abort_all();
        let pending = self.pending.clone();
        if let Ok(mut events) = self.events.lock() {
            events.take();
        }
        let (settle, promise) = Settle::new(env)?;
        match client {
            None => settle.finish(Ok(Value::Null)),
            Some(client) => {
                std::thread::Builder::new()
                    .name("push-sdk-close".to_owned())
                    .spawn(move || {
                        pending.wait_empty(CLOSE_WAIT);
                        settle.finish(block_on(client.close()).map(|()| Value::Null));
                    })
                    .map_err(|_| invalid("cannot start a thread"))?;
            }
        }
        Ok(promise)
    }

    // ----- account -----

    #[napi(catch_unwind)]
    pub fn sign_up<'env>(
        &self,
        env: &'env Env,
        email: String,
        password: String,
        handle: String,
        display_name: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client
                .sign_up(&email, &password, &handle, &display_name)
                .await
                .map(|()| Value::Null)
        })
    }

    #[napi(catch_unwind)]
    pub fn resend_verification<'env>(
        &self,
        env: &'env Env,
        email: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client
                .resend_verification(&email)
                .await
                .map(|()| Value::Null)
        })
    }

    #[napi(catch_unwind)]
    pub fn forgot_password<'env>(
        &self,
        env: &'env Env,
        email: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client.forgot_password(&email).await.map(|()| Value::Null)
        })
    }

    #[napi(catch_unwind)]
    pub fn sign_in<'env>(
        &self,
        env: &'env Env,
        email: String,
        password: String,
        device_name: String,
        platform: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let platform = json::platform(&platform)?;
            let account = client
                .sign_in(&email, &password, &device_name, platform)
                .await?;
            Ok(json::account(&account))
        })
    }

    /// The signed-in account, or null: from the bridge's cache, so it never waits.
    #[napi(catch_unwind)]
    pub fn account(&self) -> napi::Result<Value> {
        self.now(|client| {
            Ok(client
                .account()?
                .as_ref()
                .map_or(Value::Null, json::account))
        })
    }

    #[napi(catch_unwind)]
    pub fn sign_out<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client.sign_out().await.map(|()| Value::Null)
        })
    }

    // ----- connection -----

    #[napi(catch_unwind)]
    pub fn connect(&self) -> napi::Result<()> {
        self.now(Client::connect)
    }

    #[napi(catch_unwind)]
    pub fn disconnect(&self) -> napi::Result<()> {
        self.now(Client::disconnect)
    }

    #[napi(catch_unwind)]
    pub fn connection_state(&self) -> napi::Result<Value> {
        self.now(|client| Ok(json::connection_state(client.connection_state())))
    }

    #[napi(catch_unwind)]
    pub fn wait_connected<'env>(
        &self,
        env: &'env Env,
        timeout_ms: u32,
    ) -> napi::Result<Object<'env>> {
        let timeout = std::time::Duration::from_millis(u64::from(timeout_ms));
        self.promise(env, |client| async move {
            client.wait_connected(timeout).await.map(|()| Value::Null)
        })
    }

    /// Encrypts the database with a new key of 32 bytes, or without one decrypts it.
    #[napi(catch_unwind)]
    pub fn change_database_key<'env>(
        &self,
        env: &'env Env,
        database_key: Option<Buffer>,
    ) -> napi::Result<Object<'env>> {
        let key = database_key
            .as_deref()
            .map(DatabaseKey::from_bytes)
            .transpose()
            .map_err(|error| failure(&error))?;
        self.promise(env, |client| async move {
            client
                .change_database_key(key.as_ref())
                .await
                .map(|()| Value::Null)
        })
    }

    // ----- local data -----

    #[napi(catch_unwind)]
    pub fn conversations<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(json::list(&client.conversations()?, json::conversation))
        })
    }

    /// One conversation, or null.
    #[napi(catch_unwind)]
    pub fn conversation<'env>(&self, env: &'env Env, id: String) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let found = client.conversation(conversation_id(&id)?)?;
            Ok(found.as_ref().map_or(Value::Null, json::conversation))
        })
    }

    #[napi(catch_unwind)]
    pub fn messages<'env>(
        &self,
        env: &'env Env,
        conversation: String,
        before_seq: Option<i64>,
        limit: u32,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let messages = client.messages(conversation_id(&conversation)?, before_seq, limit)?;
            Ok(json::list(&messages, json::message))
        })
    }

    #[napi(catch_unwind)]
    pub fn contacts<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(json::list(&client.contacts()?, json::contact))
        })
    }

    // ----- sending -----

    /// Queues a message; resolves to the local echo.
    #[napi(catch_unwind)]
    pub fn send<'env>(
        &self,
        env: &'env Env,
        conversation: String,
        text: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let message = client.send(conversation_id(&conversation)?, &text)?;
            Ok(json::message(&message))
        })
    }

    /// Resolves to the change's id.
    #[napi(catch_unwind)]
    pub fn edit<'env>(
        &self,
        env: &'env Env,
        conversation: String,
        target: String,
        text: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let target: ClientMsgId = json::id(&target, "clientMsgId")?;
            let change = client.edit(conversation_id(&conversation)?, target, &text)?;
            Ok(Value::String(change.to_string()))
        })
    }

    /// Resolves to the change's id.
    #[napi(catch_unwind)]
    pub fn delete<'env>(
        &self,
        env: &'env Env,
        conversation: String,
        target: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let target: ClientMsgId = json::id(&target, "clientMsgId")?;
            let change = client.delete(conversation_id(&conversation)?, target)?;
            Ok(Value::String(change.to_string()))
        })
    }

    /// Resolves to whether the message was taken back.
    #[napi(catch_unwind)]
    pub fn cancel<'env>(
        &self,
        env: &'env Env,
        conversation: String,
        message: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let message: ClientMsgId = json::id(&message, "clientMsgId")?;
            Ok(Value::Bool(
                client.cancel(conversation_id(&conversation)?, message)?,
            ))
        })
    }

    #[napi(catch_unwind)]
    pub fn mark_read<'env>(
        &self,
        env: &'env Env,
        conversation: String,
        seq: i64,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client
                .mark_read(conversation_id(&conversation)?, seq)
                .map(|()| Value::Null)
        })
    }

    // ----- requests -----

    #[napi(catch_unwind)]
    pub fn list_conversations<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(json::list(
                &client.list_conversations().await?,
                json::conversation,
            ))
        })
    }

    #[napi(catch_unwind)]
    pub fn fetch_history<'env>(
        &self,
        env: &'env Env,
        conversation: String,
        before_seq: Option<i64>,
        limit: u32,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let id = conversation_id(&conversation)?;
            let messages = client.fetch_history(id, before_seq, limit).await?;
            Ok(json::list(&messages, json::message))
        })
    }

    #[napi(catch_unwind)]
    pub fn open_direct<'env>(&self, env: &'env Env, peer: String) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let conversation = client.open_direct(user_id(&peer)?).await?;
            Ok(json::conversation(&conversation))
        })
    }

    #[napi(catch_unwind)]
    pub fn list_contacts<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(json::list(&client.list_contacts().await?, json::contact))
        })
    }

    /// `request`, `accept`, `decline`, `block` or `unblock`; resolves to the relationship.
    #[napi(catch_unwind)]
    pub fn change_contact<'env>(
        &self,
        env: &'env Env,
        change: String,
        user: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let user = user_id(&user)?;
            let contact = match change.as_str() {
                "request" => client.request_contact(user).await?,
                "accept" => client.accept_contact(user).await?,
                "decline" => client.decline_contact(user).await?,
                "block" => client.block(user).await?,
                "unblock" => client.unblock(user).await?,
                _ => return Err(SdkError::Invalid(format!("no contact change {change:?}"))),
            };
            Ok(json::contact(&contact))
        })
    }

    #[napi(catch_unwind)]
    pub fn create_invite<'env>(
        &self,
        env: &'env Env,
        expires_in_s: u32,
        max_uses: u32,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(json::invite(
                &client.create_invite(expires_in_s, max_uses).await?,
            ))
        })
    }

    /// Resolves to the inviter's user id.
    #[napi(catch_unwind)]
    pub fn redeem_invite<'env>(&self, env: &'env Env, code: String) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(Value::String(
                client.redeem_invite(&code).await?.to_string(),
            ))
        })
    }

    #[napi(catch_unwind)]
    pub fn revoke_invite<'env>(&self, env: &'env Env, code: String) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client.revoke_invite(&code).await.map(|()| Value::Null)
        })
    }

    #[napi(catch_unwind)]
    pub fn profiles<'env>(&self, env: &'env Env, ids: Vec<String>) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let ids = ids
                .iter()
                .map(|id| user_id(id))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json::list(&client.profiles(ids).await?, json::profile))
        })
    }

    #[napi(catch_unwind)]
    pub fn lookup<'env>(&self, env: &'env Env, handle: String) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(json::profile(&client.lookup(&handle).await?))
        })
    }

    #[napi(catch_unwind)]
    pub fn devices<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            Ok(json::list(&client.devices().await?, json::device))
        })
    }

    #[napi(catch_unwind)]
    pub fn revoke_device<'env>(
        &self,
        env: &'env Env,
        device: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let device: DeviceId = json::id(&device, "deviceId")?;
            client.revoke_device(device).await.map(|()| Value::Null)
        })
    }
}
