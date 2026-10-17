//! The prepared devices. The file holds refresh tokens, which are credentials: it is
//! refused inside any Git working tree, written atomically and, on Unix, readable only
//! by its owner. On Windows keep it in a private directory (the directory's ACL decides).

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// The target's authority this state was prepared for.
    pub target: String,
    pub accounts: Vec<AccountState>,
    pub pairs: Vec<Pair>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub email: String,
    pub user_id: Option<Uuid>,
    pub devices: Vec<DeviceState>,
    /// Why the last preparation could not use this account.
    pub failure: Option<String>,
}

impl AccountState {
    pub fn new(email: &str) -> Self {
        Self {
            email: email.trim().to_owned(),
            user_id: None,
            devices: Vec::new(),
            failure: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    /// Saved before registering, so a retry finds the same device.
    pub registration_id: Uuid,
    pub device_id: Option<Uuid>,
    pub refresh_token: Option<String>,
    /// A run found its refresh token refused: the next preparation signs it in again.
    #[serde(default)]
    pub needs_sign_in: bool,
    /// Revoked or never registrable: kept so indices in pairs stay stable, never used.
    #[serde(default)]
    pub retired: bool,
}

impl std::fmt::Debug for DeviceState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceState")
            .field("registration_id", &self.registration_id)
            .field("device_id", &self.device_id)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("needs_sign_in", &self.needs_sign_in)
            .field("retired", &self.retired)
            .finish()
    }
}

impl DeviceState {
    pub fn new() -> Self {
        Self {
            registration_id: Uuid::new_v4(),
            device_id: None,
            refresh_token: None,
            needs_sign_in: false,
            retired: false,
        }
    }

    pub fn is_ready(&self) -> bool {
        !self.retired
            && !self.needs_sign_in
            && self.device_id.is_some()
            && self.refresh_token.is_some()
    }
}

impl Default for DeviceState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceRef {
    pub account: usize,
    pub device: usize,
}

/// Two devices of two accounts that are contacts, with their direct conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    pub sender: DeviceRef,
    pub receiver: DeviceRef,
    pub conversation_id: Uuid,
}

impl State {
    pub fn new(target: &str) -> Self {
        Self {
            version: VERSION,
            target: target.to_owned(),
            accounts: Vec::new(),
            pairs: Vec::new(),
        }
    }

    /// The state at `path`, `None` when there is none yet.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read the state file {}", path.display()));
            }
        };
        // serde_json's message can quote tokens, so it is not shown.
        let state: Self = serde_json::from_str(&text)
            .map_err(|_| anyhow::anyhow!("{} is not a push-load state file", path.display()))?;
        if state.version != VERSION {
            bail!(
                "{} has state version {}, not {VERSION}",
                path.display(),
                state.version
            );
        }
        Ok(Some(state))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("serialize the state")?;
        write_private(path, &bytes)
    }

    pub fn device(&self, at: DeviceRef) -> &DeviceState {
        &self.accounts[at.account].devices[at.device]
    }

    pub fn device_mut(&mut self, at: DeviceRef) -> &mut DeviceState {
        &mut self.accounts[at.account].devices[at.device]
    }

    fn is_ready(&self, at: DeviceRef) -> bool {
        self.accounts
            .get(at.account)
            .and_then(|account| account.devices.get(at.device))
            .is_some_and(DeviceState::is_ready)
    }

    pub fn pair_is_ready(&self, pair: &Pair) -> bool {
        self.is_ready(pair.sender) && self.is_ready(pair.receiver)
    }

    /// The ready devices: the ready pairs' first, so that any run of at least twice the
    /// pairs includes them, then by account and device.
    pub fn ready_devices(&self) -> Vec<DeviceRef> {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        let paired = self
            .pairs
            .iter()
            .filter(|pair| self.pair_is_ready(pair))
            .flat_map(|pair| [pair.sender, pair.receiver]);
        let all = self
            .accounts
            .iter()
            .enumerate()
            .flat_map(|(account, state)| {
                (0..state.devices.len()).map(move |device| DeviceRef { account, device })
            });
        for at in paired.chain(all) {
            if self.is_ready(at) && seen.insert(at) {
                ordered.push(at);
            }
        }
        ordered
    }
}

/// Refuses a credential file inside a Git working tree, where it could be committed.
pub fn ensure_outside_git(path: &Path) -> anyhow::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("the current directory")?
            .join(path)
    };
    let mut directory = absolute.parent();
    while let Some(current) = directory {
        if current.join(".git").exists() {
            bail!(
                "{} is inside the Git working tree {}: keep push-load credential files in a private directory outside every repository",
                path.display(),
                current.display()
            );
        }
        directory = current.parent();
    }
    Ok(())
}

/// Writes `bytes` to `path` through a temporary file renamed over it, owner-only on Unix.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ensure_outside_git(path)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write {}", temporary.display()))?;
    }
    std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("push-load-state-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ready_device() -> DeviceState {
        DeviceState {
            device_id: Some(Uuid::now_v7()),
            refresh_token: Some(format!("refresh-{}", Uuid::now_v7())),
            ..DeviceState::new()
        }
    }

    #[test]
    fn credential_files_are_refused_inside_a_git_working_tree() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("repo").join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("repo").join("nested")).unwrap();
        assert!(ensure_outside_git(&dir.join("repo").join("nested").join("state.json")).is_err());
        assert!(write_private(&dir.join("repo").join("state.json"), b"{}").is_err());
        ensure_outside_git(&dir.join("state.json")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_round_trips_atomically_and_redacts_tokens_in_debug() {
        let dir = scratch();
        let path = dir.join("state.json");
        assert_eq!(State::load(&path).unwrap(), None);
        let mut state = State::new("push.test:8080");
        state.accounts.push(AccountState::new("a@x.test"));
        state.accounts[0].devices.push(ready_device());
        state.save(&path).unwrap();
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), Some(state.clone()));
        let token = state.accounts[0].devices[0].refresh_token.clone().unwrap();
        assert!(!format!("{state:?}").contains(&token));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file left"
        );
        std::fs::write(&path, "not json").unwrap();
        assert!(State::load(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ready_devices_put_ready_pairs_first_and_skip_unready_devices() {
        let mut state = State::new("push.test");
        for email in ["a@x.test", "b@x.test", "c@x.test"] {
            state.accounts.push(AccountState::new(email));
        }
        state.accounts[0].devices = vec![ready_device(), ready_device()];
        state.accounts[1].devices = vec![
            DeviceState {
                needs_sign_in: true,
                ..ready_device()
            },
            ready_device(),
        ];
        state.accounts[2].devices = vec![
            DeviceState::new(),
            DeviceState {
                retired: true,
                ..ready_device()
            },
            ready_device(),
        ];
        let at = |account, device| DeviceRef { account, device };
        state.pairs.push(Pair {
            sender: at(2, 2),
            receiver: at(1, 1),
            conversation_id: Uuid::now_v7(),
        });
        // A pair with an unready device is left out.
        state.pairs.push(Pair {
            sender: at(1, 0),
            receiver: at(0, 1),
            conversation_id: Uuid::now_v7(),
        });
        assert_eq!(
            state.ready_devices(),
            [at(2, 2), at(1, 1), at(0, 0), at(0, 1)]
        );
    }
}
