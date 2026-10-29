//! The bridge's state: `state.json` next to `config.json`. Nothing in it is secret:
//! tokens stay in memory ([`BridgeSecretStore`]) and the key stays in key storage.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use push_sdk::{SecretError, SecretStore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The state file's name in the bridge's folder.
pub const FILE_NAME: &str = "state.json";

/// The SDK's secret-store key of the device it registered last.
const DEVICE_KEY: &str = "push.device";
/// The SDK's secret-store key of a device registration not answered yet.
const REGISTRATION_KEY: &str = "push.registration";

/// What the bridge remembers across restarts.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The SDK's `push.device` value, as UUID text.
    pub device_id: Option<String>,
    /// The SDK's `push.registration` value, as UUID text.
    pub registration_id: Option<String>,
    /// The agent's user id, once enrolled.
    pub enrolled_user_id: Option<String>,
    /// The current working folder.
    pub folder: Option<PathBuf>,
    /// The last session of each folder: the folder's real path (as text) → session id.
    pub sessions: BTreeMap<String, String>,
    /// The cursor of the last background message shown.
    pub agent_message_cursor: Option<u64>,
    /// The engine instance the cursor belongs to.
    pub engine_instance: Option<String>,
}

/// A state file that cannot be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("cannot read {path}: {message}")]
    Read { path: String, message: String },
    #[error("{path} is not a valid state file: {message}")]
    Parse { path: String, message: String },
    #[error("cannot save {path}: {message}")]
    Write { path: String, message: String },
}

/// [`State`] in memory, saved to its file on every change.
#[derive(Debug)]
pub struct StateFile {
    path: PathBuf,
    state: Mutex<State>,
}

impl StateFile {
    /// The state in `path`; an empty state when the file does not exist yet.
    pub fn load(path: &Path) -> Result<Self, StateError> {
        let state = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| StateError::Parse {
                path: path.display().to_string(),
                message: error.to_string(),
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => {
                return Err(StateError::Read {
                    path: path.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        Ok(Self {
            path: path.to_owned(),
            state: Mutex::new(state),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A copy of the current state.
    pub fn get(&self) -> State {
        self.lock().clone()
    }

    /// Changes the state and saves it when it changed. When the save fails the state
    /// stays as it was.
    pub fn update<R>(&self, change: impl FnOnce(&mut State) -> R) -> Result<R, StateError> {
        let mut state = self.lock();
        let mut changed = state.clone();
        let result = change(&mut changed);
        if changed != *state {
            self.save(&changed)?;
            *state = changed;
        }
        Ok(result)
    }

    /// Writes `state.json.tmp`, then renames it over `state.json`: a crash leaves the old
    /// file or the new one, never half of one.
    fn save(&self, state: &State) -> Result<(), StateError> {
        let failed = |error: std::io::Error| StateError::Write {
            path: self.path.display().to_string(),
            message: error.to_string(),
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(failed)?;
        }
        let mut temporary = self.path.clone().into_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let json = serde_json::to_vec_pretty(state).expect("state serializes");
        let written = std::fs::File::create(&temporary).and_then(|mut file| {
            std::io::Write::write_all(&mut file, &json)?;
            file.sync_all()
        });
        written
            .and_then(|()| std::fs::rename(&temporary, &self.path))
            .map_err(|error| {
                let _ = std::fs::remove_file(&temporary);
                failed(error)
            })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("not poisoned")
    }
}

/// A [`push_sdk::SecretStore`] that keeps tokens in memory only and persists the device
/// and registration ids (not secret) in the state file, so a restart signs the same
/// device in again. The SDK stores both ids as the 16 bytes of a UUID; the state file
/// keeps them as UUID text.
pub struct BridgeSecretStore {
    file: Arc<StateFile>,
    memory: Mutex<HashMap<String, Vec<u8>>>,
}

impl BridgeSecretStore {
    pub fn new(file: Arc<StateFile>) -> Self {
        Self {
            file,
            memory: Mutex::new(HashMap::new()),
        }
    }

    fn id_field(key: &str) -> Option<fn(&mut State) -> &mut Option<String>> {
        match key {
            DEVICE_KEY => Some(|state| &mut state.device_id),
            REGISTRATION_KEY => Some(|state| &mut state.registration_id),
            _ => None,
        }
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.memory.lock().expect("not poisoned")
    }
}

impl fmt::Debug for BridgeSecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BridgeSecretStore { .. }")
    }
}

impl SecretStore for BridgeSecretStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        let Some(field) = Self::id_field(key) else {
            return Ok(self.memory().get(key).cloned());
        };
        let mut state = self.file.get();
        Ok(field(&mut state)
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .map(|id| id.as_bytes().to_vec()))
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        let Some(field) = Self::id_field(key) else {
            self.memory().insert(key.to_owned(), value.to_vec());
            return Ok(());
        };
        let id = Uuid::from_slice(value)
            .map_err(|_| SecretError(format!("{key} is not a UUID")))?
            .to_string();
        self.file
            .update(|state| *field(state) = Some(id))
            .map_err(|error| SecretError(error.to_string()))
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let Some(field) = Self::id_field(key) else {
            self.memory().remove(key);
            return Ok(());
        };
        self.file
            .update(|state| *field(state) = None)
            .map_err(|error| SecretError(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    fn state_path(dir: &TestDir) -> PathBuf {
        dir.path().join("nested").join(FILE_NAME)
    }

    #[test]
    fn a_missing_file_is_an_empty_state() {
        let dir = TestDir::new();
        let file = StateFile::load(&state_path(&dir)).unwrap();
        assert_eq!(file.get(), State::default());
        assert!(!state_path(&dir).exists());
    }

    #[test]
    fn the_state_survives_a_restart() {
        let dir = TestDir::new();
        let path = state_path(&dir);
        let file = StateFile::load(&path).unwrap();
        let returned = file
            .update(|state| {
                state.folder = Some(PathBuf::from("/work/app"));
                state
                    .sessions
                    .insert("/work/app".into(), "session-1".into());
                state.agent_message_cursor = Some(7);
                state.engine_instance = Some("engine-1".into());
                state.enrolled_user_id = Some("user-1".into());
                42
            })
            .unwrap();
        assert_eq!(returned, 42);
        let reloaded = StateFile::load(&path).unwrap();
        assert_eq!(reloaded.get(), file.get());
        assert_eq!(reloaded.get().sessions["/work/app"], "session-1");
    }

    #[test]
    fn a_save_leaves_no_temporary_file() {
        let dir = TestDir::new();
        let path = state_path(&dir);
        let file = StateFile::load(&path).unwrap();
        file.update(|state| state.agent_message_cursor = Some(1))
            .unwrap();
        file.update(|state| state.agent_message_cursor = Some(2))
            .unwrap();
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [FILE_NAME]);
    }

    #[test]
    fn a_failed_save_keeps_the_state_as_it_was() {
        let dir = TestDir::new();
        // A folder where the file should be: the rename fails.
        let path = dir.mkdir(FILE_NAME);
        let file = StateFile {
            path,
            state: Mutex::new(State::default()),
        };
        assert!(
            file.update(|state| state.agent_message_cursor = Some(1))
                .is_err()
        );
        assert_eq!(file.get(), State::default());
    }

    #[test]
    fn a_corrupt_file_is_an_error() {
        let dir = TestDir::new();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(
            StateFile::load(&path),
            Err(StateError::Parse { .. })
        ));
    }

    #[test]
    fn the_store_uses_the_sdks_key_names() {
        assert!(push_sdk::SECRET_KEYS.contains(&DEVICE_KEY));
        assert!(push_sdk::SECRET_KEYS.contains(&REGISTRATION_KEY));
    }

    #[test]
    fn the_secret_store_persists_ids_but_never_the_session() {
        let dir = TestDir::new();
        let path = state_path(&dir);
        let device = Uuid::new_v4();
        let registration = Uuid::now_v7();
        let session =
            br#"{"access_token":"access-token-value","refresh_token":"refresh-token-value"}"#;
        {
            let store = BridgeSecretStore::new(Arc::new(StateFile::load(&path).unwrap()));
            store.save(DEVICE_KEY, device.as_bytes()).unwrap();
            store
                .save(REGISTRATION_KEY, registration.as_bytes())
                .unwrap();
            store.save("push.session", session).unwrap();
            assert_eq!(
                store.load("push.session").unwrap().as_deref(),
                Some(&session[..])
            );
        }
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("token"), "{written}");
        assert!(written.contains(&device.to_string()));

        let file = Arc::new(StateFile::load(&path).unwrap());
        assert_eq!(file.get().device_id, Some(device.to_string()));
        let store = BridgeSecretStore::new(file.clone());
        assert_eq!(store.load("push.session").unwrap(), None);
        assert_eq!(
            store.load(DEVICE_KEY).unwrap(),
            Some(device.as_bytes().to_vec())
        );
        assert_eq!(
            store.load(REGISTRATION_KEY).unwrap(),
            Some(registration.as_bytes().to_vec())
        );

        store.delete(REGISTRATION_KEY).unwrap();
        store.delete(REGISTRATION_KEY).unwrap();
        assert_eq!(store.load(REGISTRATION_KEY).unwrap(), None);
        assert_eq!(StateFile::load(&path).unwrap().get().registration_id, None);
        store.delete("push.session").unwrap();
    }

    #[test]
    fn the_secret_store_refuses_an_id_that_is_no_uuid() {
        let dir = TestDir::new();
        let store = BridgeSecretStore::new(Arc::new(StateFile::load(&state_path(&dir)).unwrap()));
        assert!(store.save(DEVICE_KEY, b"short").is_err());
        assert_eq!(store.load(DEVICE_KEY).unwrap(), None);
    }

    #[test]
    fn debug_output_shows_no_tokens() {
        let dir = TestDir::new();
        let store = BridgeSecretStore::new(Arc::new(StateFile::load(&state_path(&dir)).unwrap()));
        store.save("push.session", b"access-token-value").unwrap();
        assert!(!format!("{store:?}").contains("access-token-value"));
    }
}
