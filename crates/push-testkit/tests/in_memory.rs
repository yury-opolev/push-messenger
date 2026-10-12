//! The behaviour suite against the in-memory store.

use push_domain::InMemoryStore;
use push_testkit::Backend;

#[derive(Debug, Clone, Copy)]
struct InMemory;

impl Backend for InMemory {
    type Store = InMemoryStore;

    async fn store(&self) -> InMemoryStore {
        InMemoryStore::new()
    }
}

fn main() {
    push_testkit::run(|| InMemory, push_testkit::cases()).exit();
}
