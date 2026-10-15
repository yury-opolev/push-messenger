//! Where the SDK keeps the device's credentials: a store the app provides, such as the
//! platform's keychain. The SDK never writes tokens anywhere else, never logs them, and
//! leaves them out of every `Debug` output.

use std::collections::HashMap;
use std::sync::Mutex;

use push_domain::{DeviceId, UserId};
use serde::{Deserialize, Serialize};

/// The key of the signed-in device's credentials.
pub(crate) const SESSION_KEY: &str = "push.session";
/// The key of the device this client registered last, kept after a sign-out by the server
/// (a password reset) so that signing in again finds the same device rather than
/// registering another.
pub(crate) const DEVICE_KEY: &str = "push.device";
/// The key of a device registration that has not been answered yet: its
/// `registration_id`, reused when the registration is retried (spec §5.3).
pub(crate) const REGISTRATION_KEY: &str = "push.registration";

/// Every key the SDK uses in the [`SecretStore`]: a store may map them to keychain items,
/// and an app that moves its data to another store copies these.
pub const SECRET_KEYS: [&str; 3] = [SESSION_KEY, DEVICE_KEY, REGISTRATION_KEY];

/// A failure of the app's secret store, in its own words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SecretError(pub String);

/// The app's secret store: a few small values under fixed keys. On a phone this is the
/// keychain or keystore; on a desktop the credential manager.
///
/// The methods are blocking and called from SDK threads; they should return quickly.
/// Values are opaque bytes; the SDK writes the credentials before it uses them, so a
/// store that fails to save never leaves the SDK holding tokens it could lose.
pub trait SecretStore: Send + Sync {
    /// The value under `key`, if any.
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError>;
    /// Replaces the value under `key`.
    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError>;
    /// Removes the value under `key`; removing a missing key succeeds.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// A secret store in memory, lost when the process ends: for tests and tools.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for MemorySecretStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MemorySecretStore { .. }")
    }
}

impl SecretStore for MemorySecretStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        Ok(self.values.lock().expect("not poisoned").get(key).cloned())
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        self.values
            .lock()
            .expect("not poisoned")
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.values.lock().expect("not poisoned").remove(key);
        Ok(())
    }
}

/// A signed-in device's credentials, as kept in the secret store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Session {
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub access_token: String,
    /// When the access token expires, in seconds since the Unix epoch.
    pub access_expires_at: i64,
    pub refresh_token: String,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("access_token", &"<redacted>")
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

impl Session {
    pub fn load(store: &dyn SecretStore) -> Result<Option<Self>, SecretError> {
        match store.load(SESSION_KEY)? {
            // An unreadable value is no session: the user signs in again.
            Some(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            None => Ok(None),
        }
    }

    pub fn save(&self, store: &dyn SecretStore) -> Result<(), SecretError> {
        let bytes = serde_json::to_vec(self).map_err(|error| SecretError(error.to_string()))?;
        store.save(SESSION_KEY, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            user_id: UserId::new(),
            device_id: DeviceId::new(),
            access_token: "access-token-value".to_owned(),
            access_expires_at: 1_900_000_000,
            refresh_token: "refresh-token-value".to_owned(),
        }
    }

    #[test]
    fn a_session_survives_the_store() {
        let store = MemorySecretStore::new();
        assert_eq!(Session::load(&store), Ok(None));
        let saved = session();
        saved.save(&store).unwrap();
        assert_eq!(Session::load(&store), Ok(Some(saved)));
        store.delete(SESSION_KEY).unwrap();
        store.delete(SESSION_KEY).unwrap();
        assert_eq!(Session::load(&store), Ok(None));
    }

    #[test]
    fn an_unreadable_session_is_no_session() {
        let store = MemorySecretStore::new();
        store.save(SESSION_KEY, b"not json").unwrap();
        assert_eq!(Session::load(&store), Ok(None));
    }

    /// docs/sdk-secure-storage.md tells app developers which keys to expect.
    #[test]
    fn the_guide_names_every_secret_key() {
        let guide = include_str!("../../../docs/sdk-secure-storage.md");
        for key in SECRET_KEYS {
            assert!(guide.contains(&format!("`{key}`")), "{key}");
        }
    }

    #[test]
    fn debug_output_never_shows_a_token() {
        let shown = format!("{:?}", session());
        assert!(!shown.contains("access-token-value"));
        assert!(!shown.contains("refresh-token-value"));
        assert!(shown.contains("access_expires_at"));
    }
}
