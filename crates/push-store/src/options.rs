//! Connection settings for a service's pool.

use std::time::Duration;

/// How [`PgStore::connect`](crate::PgStore::connect) builds its pool, and the timeouts
/// every connection gets for its whole life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOptions {
    /// Most connections the pool opens.
    pub max_connections: u32,
    /// How long a caller waits for a free connection before the call fails.
    pub acquire_timeout: Duration,
    /// How long a statement waits for a lock (`lock_timeout`). A statement that waits
    /// longer fails with SQLSTATE 55P03, reported as the retryable `Conflict`.
    pub lock_timeout: Duration,
    /// How long one statement may run (`statement_timeout`).
    pub statement_timeout: Duration,
    /// How long a transaction may sit idle before the server ends the session
    /// (`idle_in_transaction_session_timeout`), so a stuck caller cannot hold locks.
    pub idle_in_transaction_timeout: Duration,
}

impl Default for StoreOptions {
    /// 10 connections, 5 s to acquire one, 2 s lock timeout, 10 s statement timeout,
    /// 30 s idle-in-transaction timeout.
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
            lock_timeout: Duration::from_secs(2),
            statement_timeout: Duration::from_secs(10),
            idle_in_transaction_timeout: Duration::from_secs(30),
        }
    }
}

impl StoreOptions {
    /// The `SET` statements run on every new connection. Values are whole milliseconds.
    pub(crate) fn session_settings(&self) -> String {
        format!(
            "SET lock_timeout = {}; SET statement_timeout = {}; \
             SET idle_in_transaction_session_timeout = {}",
            self.lock_timeout.as_millis(),
            self.statement_timeout.as_millis(),
            self.idle_in_transaction_timeout.as_millis(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_settings_set_every_timeout_in_milliseconds() {
        let options = StoreOptions {
            lock_timeout: Duration::from_millis(250),
            ..StoreOptions::default()
        };
        assert_eq!(
            options.session_settings(),
            "SET lock_timeout = 250; SET statement_timeout = 10000; \
             SET idle_in_transaction_session_timeout = 30000"
        );
    }
}
