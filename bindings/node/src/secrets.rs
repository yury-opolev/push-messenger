//! The app's secret store, written in JavaScript, as the SDK's [`SecretStore`].
//!
//! The SDK calls its store with blocking calls on its own threads, while the JavaScript
//! store answers on the JavaScript thread, often asynchronously (a keychain module returns
//! promises). A blocking call into JavaScript from a thread the JavaScript thread waits for
//! would deadlock, so the bridge keeps these rules:
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
//! - **A write answered late still counts.** One the app confirms after the 30 s has
//!   failed the SDK's call, but the app's store did carry it out, so the cache follows it
//!   (see [`crate::ledger`]); a late failure changes nothing.
//!
//! The app's store must not be changed behind the SDK's back while a client is open, and
//! its methods must not call the client: the answer waits for the JavaScript thread, which
//! the client's call would wait for in turn, until the 30 s are over.

use std::sync::Arc;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use napi::Status;
use napi::bindgen_prelude::{Buffer, FnArgs, Function};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use push_sdk::{SECRET_KEYS, SdkError, SecretError, SecretStore};

use crate::ledger::{Cache, ask};

pub use crate::ledger::complete;

/// How long an SDK thread waits for the app's store to answer a write. A keychain may ask
/// the user first. The loads before the client opens share one such deadline.
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

type Call = ThreadsafeFunction<SecretCall, (), SecretArgs, Status, false, true>;

/// The app's JavaScript store, with the values it holds.
pub struct SecretBridge {
    call: Call,
    cache: Arc<Cache>,
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
            cache: Arc::default(),
            js_thread: std::thread::current().id(),
        })
    }

    /// Loads every value the SDK may read into the cache. Called before the client opens,
    /// on the opening thread. The three loads share one deadline of 30 s.
    pub fn preload(&self) -> Result<(), SdkError> {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        for key in SECRET_KEYS {
            let left = deadline.saturating_duration_since(Instant::now());
            let value = self
                .ask(left, "load", key, None)
                .map_err(|error| SdkError::Secrets(error.0))?;
            if let Some(value) = value {
                self.cache.preload(key, value);
            }
        }
        Ok(())
    }

    /// Asks the JavaScript store and waits for its answer, off the JavaScript thread.
    fn ask(
        &self,
        timeout: Duration,
        operation: &'static str,
        key: &str,
        value: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, SecretError> {
        if std::thread::current().id() == self.js_thread {
            return Err(SecretError(
                "the secret store was called on the JavaScript thread".to_owned(),
            ));
        }
        ask(&self.cache, timeout, operation, key, value, |id| {
            let call = SecretCall {
                id,
                operation,
                key: key.to_owned(),
                value: value.map(<[u8]>::to_vec),
            };
            match self
                .call
                .call(call, ThreadsafeFunctionCallMode::NonBlocking)
            {
                Status::Ok => Ok(()),
                _ => Err("Node.js is shutting down".to_owned()),
            }
        })
    }
}

impl SecretStore for SecretBridge {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        Ok(self.cache.get(key))
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        self.ask(ANSWER_TIMEOUT, "save", key, Some(value))
            .map(|_| ())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.ask(ANSWER_TIMEOUT, "delete", key, None).map(|_| ())
    }
}
