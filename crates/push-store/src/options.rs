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
    /// Checks that every timeout is between 1 ms and `i32::MAX` ms. PostgreSQL reads 0 as
    /// "disabled" and rejects values above `i32::MAX`; a timeout must never silently turn
    /// off. The error names the offending setting.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("lock_timeout", self.lock_timeout),
            ("statement_timeout", self.statement_timeout),
            (
                "idle_in_transaction_timeout",
                self.idle_in_transaction_timeout,
            ),
        ] {
            let millis = value.as_millis();
            if millis < 1 {
                return Err(format!(
                    "{name} is {value:?}; it must be at least 1 ms (0 would disable it)"
                ));
            }
            if millis > i32::MAX as u128 {
                return Err(format!(
                    "{name} is {value:?}; it must be at most {} ms",
                    i32::MAX
                ));
            }
        }
        Ok(())
    }

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
    fn a_timeout_below_one_millisecond_is_refused_by_name() {
        for (name, options) in [
            (
                "lock_timeout",
                StoreOptions {
                    lock_timeout: Duration::ZERO,
                    ..StoreOptions::default()
                },
            ),
            (
                "statement_timeout",
                StoreOptions {
                    statement_timeout: Duration::from_micros(999),
                    ..StoreOptions::default()
                },
            ),
            (
                "idle_in_transaction_timeout",
                StoreOptions {
                    idle_in_transaction_timeout: Duration::ZERO,
                    ..StoreOptions::default()
                },
            ),
        ] {
            let message = options.validate().unwrap_err();
            assert!(message.contains(name), "{message}");
        }
        let one_millisecond = StoreOptions {
            lock_timeout: Duration::from_millis(1),
            ..StoreOptions::default()
        };
        assert_eq!(one_millisecond.validate(), Ok(()));
    }

    #[test]
    fn a_timeout_above_i32_max_milliseconds_is_refused_by_name() {
        let limit = Duration::from_millis(i32::MAX as u64);
        let too_long = limit + Duration::from_millis(1);
        for (name, options) in [
            (
                "lock_timeout",
                StoreOptions {
                    lock_timeout: too_long,
                    ..StoreOptions::default()
                },
            ),
            (
                "statement_timeout",
                StoreOptions {
                    statement_timeout: too_long,
                    ..StoreOptions::default()
                },
            ),
            (
                "idle_in_transaction_timeout",
                StoreOptions {
                    idle_in_transaction_timeout: too_long,
                    ..StoreOptions::default()
                },
            ),
        ] {
            let message = options.validate().unwrap_err();
            assert!(message.contains(name), "{message}");
        }
        let at_limit = StoreOptions {
            statement_timeout: limit,
            ..StoreOptions::default()
        };
        assert_eq!(at_limit.validate(), Ok(()));
    }

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
