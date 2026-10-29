//! The bridge's configuration: `config.json` in the bridge's folder
//! ([`default_dir`] unless `--dir` names another).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::PermissionMode;
use crate::folders;

/// The configuration file's name in the bridge's folder.
pub const FILE_NAME: &str = "config.json";

/// One agent's bridge configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// auth-svc's public URL: `https://push.example.com`.
    pub api_url: String,
    /// The gateway's WebSocket; by default `api_url` with `https` → `wss` (`http` →
    /// `ws`) and `/v1/socket` appended.
    #[serde(default)]
    pub socket_url: Option<String>,
    /// A `Host` header override, for development clusters reached by address.
    #[serde(default)]
    pub host: Option<String>,
    /// The agent-key assertions' audience; by default `api_url` without a trailing `/`.
    #[serde(default)]
    pub audience: Option<String>,
    /// Allows `http://` and `ws://` to hosts other than this machine.
    #[serde(default)]
    pub allow_insecure_transport: bool,
    #[serde(default)]
    pub key: KeyConfig,
    /// The device's name; by default the computer's name.
    #[serde(default)]
    pub device_name: Option<String>,
    /// The folders the agent may work in (and below); at least one.
    pub roots: Vec<PathBuf>,
    /// The folder a new bridge starts in: inside `roots`; a relative path is relative to
    /// the first root.
    pub default_folder: PathBuf,
    /// `default`, `acceptEdits` or `plan`; never `bypassPermissions` (the owner turns
    /// approvals off with `/mode bypass`, confirmed by a card, until `/new`).
    #[serde(default)]
    pub default_permission_mode: PermissionMode,
    pub backend: BackendKind,
    /// Required when `backend` is `coda`.
    #[serde(default)]
    pub coda: Option<CodaConfig>,
}

/// The agent's signing key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KeyConfig {
    /// The key's name in key storage: one key per agent.
    pub name: String,
    pub storage: KeyStorage,
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self {
            name: "push-messenger-coda-bridge".to_owned(),
            storage: KeyStorage::Windows,
        }
    }
}

/// Where the private key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyStorage {
    /// Windows key storage (CNG): the TPM when there is one, else the software provider.
    /// The key cannot be exported.
    Windows,
    /// `<dir>/agent-key.p8`: a plain file, for development and tests only.
    SoftwareFile,
}

/// The agent backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum BackendKind {
    Coda,
}

impl TryFrom<String> for BackendKind {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        match name.as_str() {
            "coda" => Ok(Self::Coda),
            _ => Err(format!(
                "unknown backend \"{name}\": the only backend is \"coda\""
            )),
        }
    }
}

impl From<BackendKind> for String {
    fn from(kind: BackendKind) -> Self {
        match kind {
            BackendKind::Coda => "coda".to_owned(),
        }
    }
}

/// How to start Coda: `<exe> serve <args…> --cwd <folder> --permission-mode <mode>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodaConfig {
    /// The path to `coda.exe`.
    pub exe: PathBuf,
    /// Extra arguments after `serve`.
    #[serde(default)]
    pub args: Vec<String>,
}

/// A configuration that cannot be read or is wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {message}")]
    Read { path: String, message: String },
    #[error("{path} is not a valid configuration: {message}")]
    Parse { path: String, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// The bridge's folder: `%LOCALAPPDATA%\push-messenger\coda-bridge` on Windows;
/// elsewhere `$XDG_STATE_HOME/push-messenger/coda-bridge` (by default under
/// `~/.local/state`).
pub fn default_dir() -> PathBuf {
    base_dir().join("push-messenger").join("coda-bridge")
}

#[cfg(windows)]
fn base_dir() -> PathBuf {
    if let Some(local) = non_empty_var("LOCALAPPDATA") {
        return local;
    }
    match non_empty_var("USERPROFILE") {
        Some(profile) => profile.join("AppData").join("Local"),
        None => PathBuf::from("."),
    }
}

#[cfg(not(windows))]
fn base_dir() -> PathBuf {
    if let Some(state) = non_empty_var("XDG_STATE_HOME").filter(|path| path.is_absolute()) {
        return state;
    }
    match non_empty_var("HOME") {
        Some(home) => home.join(".local").join("state"),
        None => PathBuf::from("."),
    }
}

fn non_empty_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

impl Config {
    /// Reads and parses `path` (a UTF-8 byte order mark, as Notepad writes, is fine).
    /// Does not [`validate`](Self::validate).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|error| ConfigError::Read {
            path: shown.clone(),
            message: error.to_string(),
        })?;
        Self::parse(&text).map_err(|message| ConfigError::Parse {
            path: shown,
            message,
        })
    }

    fn parse(text: &str) -> Result<Self, String> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    /// Checks what parsing cannot: the URLs' schemes, the roots, the default folder (a
    /// real folder inside the roots), the default permission mode and the backend's
    /// section.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| Err(ConfigError::Invalid(message.to_owned()));
        if !(self.api_url.starts_with("https://") || self.api_url.starts_with("http://")) {
            return invalid("api_url must start with https:// (or http://)");
        }
        if let Some(socket_url) = &self.socket_url {
            if !(socket_url.starts_with("wss://") || socket_url.starts_with("ws://")) {
                return invalid("socket_url must start with wss:// (or ws://)");
            }
        }
        if self.key.name.trim().is_empty() {
            return invalid("key.name must not be empty");
        }
        if self.default_permission_mode == PermissionMode::BypassPermissions {
            return invalid(
                "default_permission_mode cannot be bypassPermissions: the owner turns approvals off with /mode bypass",
            );
        }
        let Some(first_root) = self.roots.first() else {
            return invalid("roots must name at least one folder");
        };
        let Some(default_folder) = self.default_folder.to_str() else {
            return invalid("default_folder is not valid Unicode");
        };
        if let Err(error) = folders::resolve(default_folder, first_root, &self.roots) {
            return Err(ConfigError::Invalid(format!(
                "default_folder {}: {error}",
                self.default_folder.display()
            )));
        }
        match (self.backend, &self.coda) {
            (BackendKind::Coda, None) => invalid("backend \"coda\" needs a \"coda\" section"),
            (BackendKind::Coda, Some(coda)) if coda.exe.as_os_str().is_empty() => {
                invalid("coda.exe must name Coda's executable")
            }
            _ => Ok(()),
        }
    }

    /// [`Config::socket_url`], or the one derived from `api_url`.
    pub fn socket_url(&self) -> String {
        if let Some(url) = &self.socket_url {
            return url.clone();
        }
        let base = self.api_url.trim_end_matches('/');
        let socket = match (base.strip_prefix("https://"), base.strip_prefix("http://")) {
            (Some(rest), _) => format!("wss://{rest}"),
            (None, Some(rest)) => format!("ws://{rest}"),
            (None, None) => base.to_owned(),
        };
        format!("{socket}/v1/socket")
    }

    /// [`Config::audience`], or `api_url` without a trailing `/`.
    pub fn audience(&self) -> String {
        match &self.audience {
            Some(audience) => audience.clone(),
            None => self.api_url.trim_end_matches('/').to_owned(),
        }
    }

    /// [`Config::device_name`], or the computer's name, or `push-agent-bridge`.
    pub fn device_name(&self) -> String {
        self.device_name
            .clone()
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "push-agent-bridge".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_dir::TestDir;

    struct Fixture {
        dir: TestDir,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = TestDir::new();
        let root = dir.mkdir("work");
        dir.mkdir("work/app");
        dir.mkdir("elsewhere");
        Fixture { dir, root }
    }

    fn minimal(fixture: &Fixture) -> serde_json::Value {
        json!({
            "api_url": "https://push.example.com/",
            "roots": [fixture.root],
            "default_folder": fixture.root.join("app"),
            "backend": "coda",
            "coda": { "exe": "C:\\Tools\\coda.exe" },
        })
    }

    fn write(fixture: &Fixture, config: &serde_json::Value) -> PathBuf {
        let path = fixture.dir.path().join(FILE_NAME);
        std::fs::write(&path, config.to_string()).unwrap();
        path
    }

    fn load(fixture: &Fixture, config: &serde_json::Value) -> Result<Config, ConfigError> {
        Config::load(&write(fixture, config))
    }

    #[test]
    fn a_full_config_parses() {
        let fixture = fixture();
        let config = load(
            &fixture,
            &json!({
                "api_url": "https://push.example.com",
                "socket_url": "wss://gateway.example.com/v1/socket",
                "host": "push.local",
                "audience": "https://auth.example.com",
                "allow_insecure_transport": true,
                "key": { "name": "my-agent", "storage": "software-file" },
                "device_name": "build box",
                "roots": [fixture.root, fixture.dir.path().join("elsewhere")],
                "default_folder": "app",
                "default_permission_mode": "acceptEdits",
                "backend": "coda",
                "coda": { "exe": "coda", "args": ["--profile", "work"] },
            }),
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.socket_url(), "wss://gateway.example.com/v1/socket");
        assert_eq!(config.audience(), "https://auth.example.com");
        assert_eq!(config.device_name(), "build box");
        assert_eq!(config.host.as_deref(), Some("push.local"));
        assert!(config.allow_insecure_transport);
        assert_eq!(
            config.key,
            KeyConfig {
                name: "my-agent".into(),
                storage: KeyStorage::SoftwareFile
            }
        );
        assert_eq!(config.default_permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(
            config.coda,
            Some(CodaConfig {
                exe: "coda".into(),
                args: vec!["--profile".into(), "work".into()]
            })
        );
    }

    #[test]
    fn a_minimal_config_takes_the_defaults() {
        let fixture = fixture();
        let config = load(&fixture, &minimal(&fixture)).unwrap();
        config.validate().unwrap();
        assert_eq!(config.socket_url, None);
        assert_eq!(config.socket_url(), "wss://push.example.com/v1/socket");
        assert_eq!(config.audience(), "https://push.example.com");
        assert_eq!(config.key, KeyConfig::default());
        assert_eq!(config.key.name, "push-messenger-coda-bridge");
        assert_eq!(config.key.storage, KeyStorage::Windows);
        assert!(!config.allow_insecure_transport);
        assert_eq!(config.default_permission_mode, PermissionMode::Default);
        assert_eq!(config.backend, BackendKind::Coda);
        assert!(!config.device_name().is_empty());
    }

    #[test]
    fn a_byte_order_mark_is_fine() {
        let fixture = fixture();
        let path = fixture.dir.path().join(FILE_NAME);
        std::fs::write(&path, format!("\u{feff}{}", minimal(&fixture))).unwrap();
        assert!(Config::load(&path).is_ok());
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let fixture = fixture();
        let mut config = minimal(&fixture);
        config["api_ulr"] = json!("https://typo.example.com");
        let error = load(&fixture, &config).unwrap_err();
        assert!(
            matches!(&error, ConfigError::Parse { message, .. } if message.contains("api_ulr")),
            "{error}"
        );
        let mut config = minimal(&fixture);
        config["key"]["storage_kind"] = json!("windows");
        assert!(load(&fixture, &config).is_err());
    }

    #[test]
    fn an_unknown_backend_is_refused_by_name() {
        let fixture = fixture();
        let mut config = minimal(&fixture);
        config["backend"] = json!("cortex");
        let error = load(&fixture, &config).unwrap_err().to_string();
        assert!(
            error.contains("unknown backend \"cortex\": the only backend is \"coda\""),
            "{error}"
        );
    }

    #[test]
    fn a_missing_file_says_where_it_looked() {
        let fixture = fixture();
        let path = fixture.dir.path().join("nope.json");
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(error.contains("nope.json"), "{error}");
    }

    #[test]
    fn the_socket_url_is_derived_from_the_api_url() {
        let fixture = fixture();
        let mut config = load(&fixture, &minimal(&fixture)).unwrap();
        config.api_url = "http://localhost:8080".into();
        assert_eq!(config.socket_url(), "ws://localhost:8080/v1/socket");
        config.api_url = "https://push.example.com//".into();
        assert_eq!(config.socket_url(), "wss://push.example.com/v1/socket");
        assert_eq!(config.audience(), "https://push.example.com");
        config.socket_url = Some("wss://other.example.com/socket".into());
        assert_eq!(config.socket_url(), "wss://other.example.com/socket");
    }

    #[test]
    fn bypass_is_never_the_default_mode() {
        let fixture = fixture();
        let mut config = minimal(&fixture);
        config["default_permission_mode"] = json!("bypassPermissions");
        let error = load(&fixture, &config).unwrap().validate().unwrap_err();
        assert!(error.to_string().contains("bypassPermissions"), "{error}");
    }

    #[test]
    fn the_default_folder_must_be_inside_the_roots() {
        let fixture = fixture();
        let mut config = minimal(&fixture);
        config["default_folder"] = json!(fixture.dir.path().join("elsewhere"));
        let error = load(&fixture, &config).unwrap().validate().unwrap_err();
        assert!(
            error.to_string().contains("outside the allowed roots"),
            "{error}"
        );
        config["default_folder"] = json!(fixture.root.join("missing"));
        let error = load(&fixture, &config).unwrap().validate().unwrap_err();
        assert!(error.to_string().contains("no such folder"), "{error}");
    }

    #[test]
    fn other_mistakes_are_refused() {
        let fixture = fixture();
        let refused = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut config = minimal(&fixture);
            change(&mut config);
            load(&fixture, &config).unwrap().validate().is_err()
        };
        assert!(refused(&|config| config["roots"] = json!([])));
        assert!(refused(&|config| {
            config.as_object_mut().unwrap().remove("coda");
        }));
        assert!(refused(&|config| config["coda"]["exe"] = json!("")));
        assert!(refused(
            &|config| config["api_url"] = json!("push.example.com")
        ));
        assert!(refused(
            &|config| config["socket_url"] = json!("https://x/v1/socket")
        ));
        assert!(refused(&|config| config["key"] = json!({ "name": " " })));
    }
}
