//! The calls a client has running, so `close` can abort them and wait until their tasks
//! have dropped their clones of the client (`Client::close` needs the last one).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::task::AbortHandle;

#[derive(Default)]
struct State {
    closed: bool,
    next_id: u64,
    // `None` between registering a task and getting its handle from the runtime.
    tasks: HashMap<u64, Option<AbortHandle>>,
}

#[derive(Default)]
pub struct Pending {
    state: Mutex<State>,
    emptied: Condvar,
}

impl Pending {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a task before it is spawned; `None` once `abort_all` ran.
    pub fn register(self: &Arc<Self>) -> Option<Registration> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        let id = state.next_id;
        state.next_id += 1;
        state.tasks.insert(id, None);
        Some(Registration {
            id,
            pending: self.clone(),
        })
    }

    /// Stops accepting tasks and aborts the running ones.
    pub fn abort_all(&self) {
        let mut state = self.lock();
        state.closed = true;
        for handle in state.tasks.values().flatten() {
            handle.abort();
        }
    }

    /// Records a registered task's handle; aborts the task at once if `abort_all` ran
    /// meanwhile.
    pub fn started(&self, id: u64, handle: AbortHandle) {
        let mut state = self.lock();
        if state.closed {
            handle.abort();
        }
        // Gone already when the task finished before this ran.
        if let Some(slot) = state.tasks.get_mut(&id) {
            *slot = Some(handle);
        }
    }

    /// Waits until every task has been dropped, at most `timeout`.
    pub fn wait_empty(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while !state.tasks.is_empty() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            state = self
                .emptied
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

/// One task's place in the set; leaving it (when the task's future is dropped, finished or
/// aborted) removes the task.
pub struct Registration {
    id: u64,
    pending: Arc<Pending>,
}

impl Registration {
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut state = self.pending.lock();
        state.tasks.remove(&self.id);
        if state.tasks.is_empty() {
            self.pending.emptied.notify_all();
        }
    }
}

/// A future that leaves the set only after the future inside it is dropped, together with
/// everything that future holds (field order is drop order).
pub struct Tracked {
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
    _registration: Registration,
}

impl Tracked {
    pub fn new(
        future: impl Future<Output = ()> + Send + 'static,
        registration: Registration,
    ) -> Self {
        Self {
            future: Box::pin(future),
            _registration: registration,
        }
    }
}

impl Future for Tracked {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        self.future.as_mut().poll(context)
    }
}
