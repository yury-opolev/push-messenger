//! Opens the agent's key as `config.json`'s `key` section says.

use std::path::Path;
use std::sync::Arc;

use push_agent_key::software::SoftwareKey;
use push_agent_key::{AgentKey, AgentPublicKey, AgentSigner, SignerError};
use push_bridge::config::{KeyConfig, KeyStorage};

/// The software key's file in the bridge's folder.
pub const SOFTWARE_KEY_FILE: &str = "agent-key.p8";

/// An opened key: the signer for the SDK, and what to show people.
pub struct Key {
    pub signer: Arc<dyn AgentSigner>,
    pub public_key: AgentPublicKey,
    /// Where the private key lives.
    pub storage: String,
}

impl Key {
    fn new<K: AgentKey + 'static>(key: K) -> Self {
        let public_key = key.public_key();
        let storage = key.storage();
        Self {
            signer: Arc::new(key),
            public_key,
            storage,
        }
    }

    /// The RFC 7638 thumbprint auth-svc knows the agent by.
    pub fn thumbprint(&self) -> String {
        self.public_key.thumbprint()
    }
}

/// Why the key could not be opened.
#[derive(Debug)]
pub enum KeyError {
    /// There is no key yet: run `keygen`.
    Missing(String),
    /// The key provider failed; its code, never key material.
    Signer(SignerError),
}

/// Opens the key in `config`'s storage; with `create`, makes a new one when there is none.
pub fn open_key(config: &KeyConfig, dir: &Path, create: bool) -> Result<Key, KeyError> {
    match config.storage {
        KeyStorage::SoftwareFile => {
            let path = dir.join(SOFTWARE_KEY_FILE);
            if !create && !path.exists() {
                return Err(KeyError::Missing(format!(
                    "there is no key file {}",
                    path.display()
                )));
            }
            eprintln!(
                "warning: the key is a plain file ({}), for development only; use \"storage\": \"windows\" for real agents",
                path.display()
            );
            SoftwareKey::load_or_create(&path)
                .map(Key::new)
                .map_err(KeyError::Signer)
        }
        KeyStorage::Windows => open_windows(&config.name, create),
    }
}

#[cfg(windows)]
fn open_windows(name: &str, create: bool) -> Result<Key, KeyError> {
    use push_agent_key::cng::CngKey;

    if create {
        return CngKey::open_or_create(name)
            .map(Key::new)
            .map_err(KeyError::Signer);
    }
    CngKey::open(name).map(Key::new).map_err(|error| {
        let missing = ["NTE_BAD_KEYSET", "NTE_NOT_FOUND", "NTE_NO_KEY"]
            .iter()
            .any(|code| error.code.starts_with(code));
        if missing {
            KeyError::Missing(format!(
                "there is no key named {name} in Windows key storage"
            ))
        } else {
            KeyError::Signer(error)
        }
    })
}

#[cfg(not(windows))]
fn open_windows(_name: &str, _create: bool) -> Result<Key, KeyError> {
    Err(KeyError::Signer(SignerError {
        code: "unsupported".to_owned(),
        message: "Windows key storage is only available on Windows; use \"storage\": \"software-file\" for development".to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn software() -> KeyConfig {
        KeyConfig {
            name: "unused".to_owned(),
            storage: KeyStorage::SoftwareFile,
        }
    }

    #[test]
    fn a_software_key_is_created_once_and_then_reopened() {
        let dir = std::env::temp_dir().join(format!("pab-signer-{}", uuid::Uuid::new_v4()));
        assert!(matches!(
            open_key(&software(), &dir, false),
            Err(KeyError::Missing(_))
        ));
        let created = open_key(&software(), &dir, true).unwrap();
        let opened = open_key(&software(), &dir, false).unwrap();
        assert_eq!(created.thumbprint(), opened.thumbprint());
        assert!(
            opened.storage.contains(SOFTWARE_KEY_FILE),
            "{}",
            opened.storage
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
