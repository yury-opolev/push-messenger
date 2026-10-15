//! The app's secret store, written in JavaScript, as the SDK's [`SecretStore`].
//!
//! The SDK calls its store with blocking calls on its own threads, while the JavaScript
//! store answers on the JavaScript thread, often asynchronously (a keychain module returns
//! promises). A blocking call into JavaScript from a thread the JavaScript thread waits for
//! would deadlock, so the bridge keeps two rules:
//!
//! - **Reads never reach JavaScript.** The three values are loaded once, before the client
//!   opens (on the opening thread, while the JavaScript thread is free), and kept in a
//!   cache. `load` answers from it, so the calls JavaScript makes synchronously
//!   (`account()`) never wait for JavaScript.
//! - **Writes wait for JavaScript, but never on the JavaScript thread.** `save` and
//!   `delete` post the call through a threadsafe function and wait, up to 30 s, for
//!   `completeSecretCall`; the cache changes only once the app confirmed the write, so the
//!   SDK's rule that credentials are saved before they are used still holds. Every call
//!   that can write (sign-in, refresh, sign-out) runs on an SDK thread and returns a promise,
//!   so the JavaScript thread is free to answer. A write on the JavaScript thread itself is
//!   refused rather than deadlocked.
//!
//! The app's store must not be changed behind the SDK's back while a client is open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{LazyLock, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use napi::Status;
use napi::bindgen_prelude::{Buffer, FnArgs, Function};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use push_sdk::{SECRET_KEYS, SdkError, SecretError, SecretStore};

/// How long an SDK thread waits for the app's store to answer a write. A keychain may ask
/// the user first.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// What JavaScript receives: `(id, operation, key, value)`; it answers with
/// `completeSecretCall(id, error, value)`.
pub type SecretArgs = FnArgs<(u32, String, String, Option<Buffer>)>;

/// One call, as it waits in the threadsafe function's queue.
pub struct SecretCall {
    id: u32,
    operation: &'static str,
    key: String,
    value: Option<Vec<u8>>,
}

type Answer = Result<Option<Vec<u8>>, String>;

/// The SDK threads waiting for an answer, by call id.
static WAITING: LazyLock<Mutex<HashMap<u32, SyncSender<Answer>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Wakes the SDK thread waiting for call `id`; an answer nobody waits for (it timed out) is
/// dropped.
pub fn complete(id: u32, error: Option<String>, value: Option<Vec<u8>>) {
    let waiting = WAITING.lock().map(|mut waiting| waiting.remove(&id));
    if let Ok(Some(sender)) = waiting {
        let _ = sender.send(match error {
            Some(error) => Err(error),
            None => Ok(value),
        });
    }
}

type Call = ThreadsafeFunction<SecretCall, (), SecretArgs, Status, false, true>;

/// The app's JavaScript store, with the values it holds.
pub struct SecretBridge {
    call: Call,
    cache: Mutex<HashMap<String, Vec<u8>>>,
    js_thread: ThreadId,
}

impl SecretBridge {
    /// Created on the JavaScript thread. The threadsafe function is weak: on its own it
    /// does not keep Node running.
    pub fn new(function: &Function<'_, SecretArgs, ()>) -> napi::Result<Self> {
        let call = function
            .build_threadsafe_function::<SecretCall>()
            .callee_handled::<false>()
            .weak::<true>()
            .build_callback(|context| {
                let call = context.value;
                Ok(FnArgs::from((
                    call.id,
                    call.operation.to_owned(),
                    call.key,
                    call.value.map(Buffer::from),
                )))
            })?;
        Ok(Self {
            call,
            cache: Mutex::new(HashMap::new()),
            js_thread: std::thread::current().id(),
        })
    }

    /// Loads every value the SDK may read into the cache. Called before the client opens,
    /// on the opening thread.
    pub fn preload(&self) -> Result<(), SdkError> {
        for key in SECRET_KEYS {
            let value = self
                .ask("load", key, None)
                .map_err(|error| SdkError::Secrets(error.0))?;
            if let Some(value) = value {
                self.cache().insert(key.to_owned(), value);
            }
        }
        Ok(())
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Asks the JavaScript store and waits for its answer, off the JavaScript thread.
    fn ask(
        &self,
        operation: &'static str,
        key: &str,
        value: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, SecretError> {
        if std::thread::current().id() == self.js_thread {
            return Err(SecretError(
                "the secret store was called on the JavaScript thread".to_owned(),
            ));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (sender, answer) = sync_channel(1);
        if let Ok(mut waiting) = WAITING.lock() {
            waiting.insert(id, sender);
        }
        let call = SecretCall {
            id,
            operation,
            key: key.to_owned(),
            value: value.map(<[u8]>::to_vec),
        };
        if self
            .call
            .call(call, ThreadsafeFunctionCallMode::NonBlocking)
            != Status::Ok
        {
            complete(id, Some("Node.js is shutting down".to_owned()), None);
        }
        // On a runtime worker, the worker's other tasks move elsewhere while it waits.
        let answer = wait(|| answer.recv_timeout(ANSWER_TIMEOUT));
        match answer {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(SecretError(format!(
                "the app's secret store failed: {error}"
            ))),
            Err(_) => {
                complete(id, None, None);
                Err(SecretError(format!(
                    "the app's secret store did not answer within {} s",
                    ANSWER_TIMEOUT.as_secs()
                )))
            }
        }
    }
}

/// Runs a blocking wait; on a worker of the client's runtime, through `block_in_place`.
fn wait<T>(blocking: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(blocking)
        }
        _ => blocking(),
    }
}

impl SecretStore for SecretBridge {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        Ok(self.cache().get(key).cloned())
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        self.ask("save", key, Some(value))?;
        self.cache().insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.ask("delete", key, None)?;
        self.cache().remove(key);
        Ok(())
    }
}
