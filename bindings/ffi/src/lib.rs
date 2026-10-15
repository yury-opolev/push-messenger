//! What push-sdk's bindings share (spec §8.5): the data and error codes they hand to apps,
//! as JSON ([`json`]), and [`block_on`], which waits for the SDK from a thread of the
//! binding's own without a second runtime.
//!
//! The bindings add no logic of their own (spec §8.1): they convert and wait.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;

pub mod json;

/// Wakes the thread that waits in [`block_on`].
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to the end on the calling thread, parking it while the future waits.
///
/// The client's futures do their work on the client's own runtime and only wait here for
/// the answer, so a binding needs no runtime of its own (spec §8.5: the core owns the only
/// one). Never call this on one of the SDK's threads ([`on_sdk_thread`]): a runtime worker
/// parked here could be the one the answer needs.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::park();
    }
}

/// Whether the calling thread is one of the SDK's runtime threads (a worker, or a blocking
/// thread running store work): where the SDK calls the app's secret store from, and where
/// [`block_on`] must not wait.
pub fn on_sdk_thread() -> bool {
    tokio::runtime::Handle::try_current().is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use push_sdk::testing::{AutoGateway, FakeAuth};
    use push_sdk::{Client, Config, MemorySecretStore, Platform};

    use super::*;

    /// A plain thread waits for the client's answers; the client's runtime does the work.
    #[test]
    fn a_plain_thread_waits_for_the_client_without_a_runtime_of_its_own() {
        // The fakes' own runtime, apart from the client's.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (auth, gateway) = runtime.block_on(async {
            let auth = FakeAuth::start().await;
            let gateway = AutoGateway::start(auth.identity.clone()).await;
            (auth, gateway)
        });
        let config = Config::new(&auth.url, &gateway.url, ":memory:");
        let (client, mut events) =
            Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
        assert!(!on_sdk_thread());
        let account = block_on(client.sign_in("a@example.com", "pw", "phone", Platform::Web));
        assert_eq!(account.unwrap().user_id, auth.user);
        client.connect().unwrap();
        block_on(client.wait_connected(Duration::from_secs(10))).unwrap();
        let first = block_on(events.next()).unwrap();
        assert_eq!(json::event(&first)["type"], "connectionStateChanged");
        let inside = Arc::new(std::sync::Mutex::new(None));
        let seen = inside.clone();
        let (done, finished) = std::sync::mpsc::channel();
        client
            .spawn(async move {
                *seen.lock().unwrap() = Some(on_sdk_thread());
                let _ = done.send(());
            })
            .unwrap();
        finished.recv().unwrap();
        assert_eq!(*inside.lock().unwrap(), Some(true));
        block_on(client.close()).unwrap();
        drop(runtime);
    }

    /// Waiting for a clone to go must not need a runtime on the thread that closes.
    #[test]
    fn close_waits_for_a_clone_from_a_thread_without_a_runtime() {
        let (client, _events) = Client::open(
            Config::new("http://127.0.0.1:1", "http://127.0.0.1:1", ":memory:"),
            Arc::new(MemorySecretStore::new()),
        )
        .unwrap();
        let clone = client.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(clone);
        });
        let closing = std::thread::spawn(move || block_on(client.close()));
        assert_eq!(closing.join().expect("close must not panic"), Ok(()));
    }
}
