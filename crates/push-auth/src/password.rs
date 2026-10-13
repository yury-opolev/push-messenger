//! Password hashing with Argon2id (spec §5.6).
//!
//! Hashes are PHC strings, so they carry their parameters: a hash made with weaker
//! parameters than the current ones still verifies, and [`Checked::Right`] says when to
//! store a new hash. Hashing takes about as long as it is configured to (tens to hundreds
//! of milliseconds) and 19 MiB of memory at the OWASP minimum: callers run it on a
//! blocking thread and pass a [`HashGate`] first, which bounds how many run at once, how
//! many wait and for how long.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use tokio::sync::{Semaphore, SemaphorePermit};

/// The outcome of checking a password against a stored hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// The password does not match, or the stored hash is unreadable.
    Wrong,
    /// The password matches. `rehash` is true when the stored hash was made with other
    /// parameters than the current ones and should be replaced.
    Right { rehash: bool },
}

/// Hashes and checks passwords with one set of Argon2id parameters.
#[derive(Clone)]
pub struct Passwords {
    params: Params,
    /// A hash of a random password, checked when an account does not exist, so that an
    /// unknown email costs as much time as a wrong password.
    dummy: String,
}

impl std::fmt::Debug for Passwords {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Passwords")
            .field("memory_kib", &self.params.m_cost())
            .field("iterations", &self.params.t_cost())
            .field("parallelism", &self.params.p_cost())
            .finish()
    }
}

impl Passwords {
    /// Memory cost of the OWASP minimum, in KiB.
    pub const OWASP_MEMORY_KIB: u32 = 19_456;
    /// Iterations of the OWASP minimum.
    pub const OWASP_ITERATIONS: u32 = 2;
    /// Parallelism of the OWASP minimum.
    pub const OWASP_PARALLELISM: u32 = 1;

    /// Argon2id with `memory_kib` KiB, `iterations` passes and `parallelism` lanes.
    pub fn new(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<Self, String> {
        let params = Params::new(memory_kib, iterations, parallelism, None)
            .map_err(|error| format!("invalid Argon2id parameters: {error}"))?;
        let mut passwords = Self {
            params,
            dummy: String::new(),
        };
        passwords.dummy = passwords.hash(&random_password());
        Ok(passwords)
    }

    /// The OWASP minimum, `m=19456 KiB, t=2, p=1`, which the spec requires.
    pub fn owasp() -> Self {
        Self::new(
            Self::OWASP_MEMORY_KIB,
            Self::OWASP_ITERATIONS,
            Self::OWASP_PARALLELISM,
        )
        .expect("the OWASP parameters are valid")
    }

    fn argon2(&self) -> Argon2<'_> {
        Argon2::new(Algorithm::Argon2id, Version::V0x13, self.params.clone())
    }

    /// A PHC string for `password` with a new random salt.
    pub fn hash(&self, password: &str) -> String {
        let salt = SaltString::generate(&mut OsRng);
        self.argon2()
            .hash_password(password.as_bytes(), &salt)
            .expect("Argon2id hashing with valid parameters cannot fail")
            .to_string()
    }

    /// Checks `password` against the PHC string `stored`.
    pub fn check(&self, password: &str, stored: &str) -> Checked {
        let Ok(parsed) = PasswordHash::new(stored) else {
            tracing::error!("a stored password hash is not a PHC string");
            return Checked::Wrong;
        };
        if self
            .argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_err()
        {
            return Checked::Wrong;
        }
        let current = parsed.algorithm.as_str() == "argon2id"
            && parsed.version == Some(Version::V0x13.into())
            && Params::try_from(&parsed).is_ok_and(|params| {
                params.m_cost() == self.params.m_cost()
                    && params.t_cost() == self.params.t_cost()
                    && params.p_cost() == self.params.p_cost()
            });
        Checked::Right { rehash: !current }
    }

    /// Spends the time of one check without an account, so that "no such account" and
    /// "wrong password" take equally long.
    pub fn check_nothing(&self, password: &str) {
        let _ = self.check(password, &self.dummy);
    }
}

fn random_password() -> String {
    SaltString::generate(&mut OsRng).as_str().to_owned()
}

/// Admission to password hashing: at most `slots` hashes at once, at most `queue`
/// requests waiting for a slot, none waiting longer than `wait`. A flood of logins thus
/// fails fast with a retryable error instead of piling up memory and latency.
#[derive(Debug)]
pub struct HashGate {
    slots: Semaphore,
    waiting: AtomicUsize,
    queue: usize,
    wait: Duration,
}

/// The gate is full: the queue is at its length, or the wait ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// Counts one waiting request while it lives.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl HashGate {
    /// A gate with `slots` concurrent hashes (at least 1), `queue` waiting requests and
    /// a longest wait of `wait`.
    pub fn new(slots: usize, queue: usize, wait: Duration) -> Self {
        Self {
            slots: Semaphore::new(slots.max(1)),
            waiting: AtomicUsize::new(0),
            queue,
            wait,
        }
    }

    /// A slot, held until the permit is dropped; [`Busy`] at once when the queue is full,
    /// or after `wait` without a free slot.
    pub async fn enter(&self) -> Result<SemaphorePermit<'_>, Busy> {
        if let Ok(permit) = self.slots.try_acquire() {
            return Ok(permit);
        }
        let ahead = self.waiting.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&self.waiting);
        if ahead >= self.queue {
            return Err(Busy);
        }
        match tokio::time::timeout(self.wait, self.slots.acquire()).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(Busy),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap parameters, so the tests stay fast.
    fn cheap() -> Passwords {
        Passwords::new(64, 1, 1).unwrap()
    }

    #[test]
    fn a_hash_is_an_argon2id_phc_string_with_the_given_parameters() {
        let hash = cheap().hash("correct horse");
        assert!(hash.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{hash}");
        assert_ne!(
            hash,
            cheap().hash("correct horse"),
            "every hash has its own salt"
        );
    }

    #[test]
    fn the_right_password_matches_and_a_wrong_one_does_not() {
        let passwords = cheap();
        let hash = passwords.hash("correct horse");
        assert_eq!(
            passwords.check("correct horse", &hash),
            Checked::Right { rehash: false }
        );
        assert_eq!(passwords.check("correct horsE", &hash), Checked::Wrong);
        assert_eq!(
            passwords.check("correct horse", "not a hash"),
            Checked::Wrong
        );
    }

    #[test]
    fn a_hash_with_other_parameters_matches_and_asks_for_a_rehash() {
        let old = cheap().hash("correct horse");
        let stronger = Passwords::new(128, 2, 1).unwrap();
        assert_eq!(
            stronger.check("correct horse", &old),
            Checked::Right { rehash: true }
        );
    }

    #[test]
    fn the_owasp_parameters_are_the_spec_minimum() {
        let hash = Passwords::owasp().hash("correct horse");
        assert!(
            hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{hash}"
        );
    }

    #[test]
    fn invalid_parameters_are_refused() {
        assert!(Passwords::new(0, 1, 1).is_err());
        assert!(Passwords::new(64, 0, 1).is_err());
    }

    #[test]
    fn debug_output_shows_the_parameters_only() {
        let shown = format!("{:?}", cheap());
        assert_eq!(
            shown,
            "Passwords { memory_kib: 64, iterations: 1, parallelism: 1 }"
        );
    }

    #[tokio::test]
    async fn the_hash_gate_bounds_the_queue_and_the_wait() {
        let gate = HashGate::new(1, 1, Duration::from_millis(100));
        let held = gate.enter().await.unwrap();
        let started = std::time::Instant::now();
        let (waited, refused) = tokio::join!(gate.enter(), async {
            tokio::task::yield_now().await;
            gate.enter().await
        });
        assert!(matches!(refused, Err(Busy)), "the queue holds one");
        assert!(matches!(waited, Err(Busy)), "the slot stayed taken");
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(held);
        assert!(gate.enter().await.is_ok(), "nobody is counted as waiting");
    }

    #[tokio::test]
    async fn a_waiting_request_gets_the_slot_that_frees_up() {
        let gate = HashGate::new(1, 1, Duration::from_secs(5));
        let held = gate.enter().await.unwrap();
        let (entered, ()) = tokio::join!(gate.enter(), async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(held);
        });
        assert!(entered.is_ok());
    }
}
