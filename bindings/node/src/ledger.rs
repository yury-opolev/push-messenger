//! The calls to the app's secret store and their answers: who waits for which call, and the
//! cache of the values the app's store holds. Nothing here knows Node, so the logic runs
//! in plain Rust tests (`tests/ledger.rs`).
//!
//! A write the app's store does not answer in time fails the SDK's call, but the store may
//! still carry it out and answer later (a keychain waiting for the user). The SDK then
//! holds an old value while the store holds the new one: after a refresh-token rotation
//! the SDK would present the rotated token again, which the server takes for theft. So a
//! timed-out write is remembered, and a late success is applied to the cache. Each write
//! has a generation from a counter, taken when it starts; a write is applied only if no
//! later one has been applied to the key, so the newer write wins, as it does in the
//! store (it runs the calls in order). A late failure changes nothing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use push_sdk::SecretError;

/// What the app's store answered: the loaded value, or its error.
type Answer = Result<Option<Vec<u8>>, String>;

#[derive(Default)]
struct CacheState {
    values: HashMap<String, Vec<u8>>,
    // The generation of the last write applied to a key.
    applied: HashMap<String, u64>,
}

/// The values the app's store holds, as far as the SDK knows.
#[derive(Default)]
pub struct Cache {
    state: Mutex<CacheState>,
}

impl Cache {
    fn lock(&self) -> MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().values.get(key).cloned()
    }

    /// A value the store held when the client opened.
    pub fn preload(&self, key: &str, value: Vec<u8>) {
        self.lock().values.insert(key.to_owned(), value);
    }

    /// Applies a write the store carried out (`None`: a delete), unless a later write was
    /// applied already.
    fn apply(&self, key: &str, value: Option<Vec<u8>>, generation: u64) {
        let mut state = self.lock();
        if state
            .applied
            .get(key)
            .is_some_and(|applied| *applied > generation)
        {
            return;
        }
        state.applied.insert(key.to_owned(), generation);
        match value {
            Some(value) => state.values.insert(key.to_owned(), value),
            None => state.values.remove(key),
        };
    }
}

/// A write whose caller gave up waiting.
struct LateWrite {
    cache: Arc<Cache>,
    key: String,
    value: Option<Vec<u8>>,
    generation: u64,
}

enum Entry {
    Waiting(SyncSender<Answer>),
    Late(LateWrite),
}

/// The calls not answered yet, by id.
static WAITING: LazyLock<Mutex<HashMap<u32, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn waiting() -> MutexGuard<'static, HashMap<u32, Entry>> {
    WAITING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The app's answer to call `id`: wakes the thread waiting for it, or applies a write that
/// was answered late (the lock is held meanwhile, so an answer cannot slip between a
/// timeout and the call becoming late). An answer to an unknown call is dropped.
pub fn complete(id: u32, error: Option<String>, value: Option<Vec<u8>>) {
    let mut waiting = waiting();
    match waiting.remove(&id) {
        Some(Entry::Waiting(sender)) => {
            let _ = sender.send(match error {
                Some(error) => Err(error),
                None => Ok(value),
            });
        }
        Some(Entry::Late(late)) if error.is_none() => {
            late.cache.apply(&late.key, late.value, late.generation);
        }
        _ => {}
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

fn failed(error: &str) -> SecretError {
    SecretError(format!("the app's secret store failed: {error}"))
}

/// Makes one call (`load`, `save` or `delete`) and waits up to `timeout` for the answer.
/// `post` hands the call, under its id, to the app's store; `Err` says it cannot (Node is
/// shutting down). A `save` or `delete` the app confirms changes `cache`, also when it
/// answers after the timeout.
pub fn ask(
    cache: &Arc<Cache>,
    timeout: Duration,
    operation: &'static str,
    key: &str,
    value: Option<&[u8]>,
    post: impl FnOnce(u32) -> Result<(), String>,
) -> Result<Option<Vec<u8>>, SecretError> {
    let is_write = operation != "load";
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (sender, answer) = sync_channel(1);
    waiting().insert(id, Entry::Waiting(sender));
    if let Err(reason) = post(id) {
        complete(id, Some(reason), None);
    }
    let applied = |answer: Answer| match answer {
        Ok(loaded) => {
            if is_write {
                cache.apply(key, value.map(<[u8]>::to_vec), generation);
            }
            Ok(loaded)
        }
        Err(error) => Err(failed(&error)),
    };
    match wait(|| answer.recv_timeout(timeout)) {
        Ok(answer) => applied(answer),
        Err(_) => {
            let mut waiting = waiting();
            if waiting.remove(&id).is_none() {
                // Answered just now.
                if let Ok(answer) = answer.try_recv() {
                    return applied(answer);
                }
            } else if is_write {
                waiting.insert(
                    id,
                    Entry::Late(LateWrite {
                        cache: cache.clone(),
                        key: key.to_owned(),
                        value: value.map(<[u8]>::to_vec),
                        generation,
                    }),
                );
            }
            Err(SecretError(format!(
                "the app's secret store did not answer within {} s",
                timeout.as_secs().max(1)
            )))
        }
    }
}
