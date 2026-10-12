//! The behaviour suite of `push-domain`, written once and run against every [`Store`].
//!
//! Each scenario is an async function generic over a [`Backend`], which makes a fresh,
//! empty store for it. A test target with `harness = false` passes a backend to [`run`],
//! which runs every [`Case`] as a separately named test with its own tokio runtime, in
//! parallel. `tests/in_memory.rs` runs the suite against the in-memory store;
//! `push-store` runs the same cases against PostgreSQL.
//!
//! The scenario functions are private to this crate and reachable only through the case
//! lists, so a scenario missing from its list is dead code, which the workspace's
//! `clippy -D warnings` gate rejects.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use libtest_mimic::{Arguments, Conclusion, Trial};
use push_domain::Store;

mod contract;
mod scenarios;
mod world;

pub use world::{Person, Svc, World, bodies, delete, edit, entries_for, seqs, system_events, text};

/// Makes stores for the scenarios.
pub trait Backend: Clone + Send + Sync + 'static {
    type Store: Store + Clone + 'static;

    /// A new store holding no data, isolated from every other store.
    fn store(&self) -> impl Future<Output = Self::Store>;

    /// Called after a case passed, to release what [`Backend::store`] made. A failed case
    /// skips it; the test binary cleans up when it ends.
    fn finish(&self) -> impl Future<Output = ()> {
        async {}
    }
}

/// One named scenario.
pub struct Case<B> {
    pub name: String,
    run: fn(B) -> Pin<Box<dyn Future<Output = ()>>>,
}

impl<B> Case<B> {
    fn new(name: &str, run: fn(B) -> Pin<Box<dyn Future<Output = ()>>>) -> Self {
        Self {
            name: name.to_owned(),
            run,
        }
    }

    /// Prefixes the name with the module that holds the scenario.
    fn within(mut self, module: &str) -> Self {
        self.name = format!("{module}::{}", self.name);
        self
    }
}

/// The cases for a list of scenario functions in the current module.
macro_rules! case_list {
    ($($scenario:ident),* $(,)?) => {
        vec![$(
            $crate::Case::new(stringify!($scenario), |backend| {
                Box::pin(async move { $scenario(&backend).await })
            }),
        )*]
    };
}
pub(crate) use case_list;

/// Every case that any store must pass: the `Tx` contract, then the domain scenarios.
pub fn cases<B: Backend>() -> Vec<Case<B>> {
    let mut cases = contract::cases();
    cases.extend(scenarios::cases());
    cases
}

/// Runs `cases` as the tests of this binary and exits. Each case gets a backend from
/// `backend` and a multi-threaded tokio runtime of its own.
pub fn run<B: Backend>(
    backend: impl Fn() -> B + Send + Sync + 'static,
    cases: Vec<Case<B>>,
) -> Conclusion {
    let backend = Arc::new(backend);
    let trials = cases
        .into_iter()
        .map(|case| {
            let backend = Arc::clone(&backend);
            Trial::test(case.name, move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("a tokio runtime");
                let backend = backend();
                runtime.block_on(async {
                    (case.run)(backend.clone()).await;
                    backend.finish().await;
                });
                Ok(())
            })
        })
        .collect();
    libtest_mimic::run(&Arguments::from_args(), trials)
}
