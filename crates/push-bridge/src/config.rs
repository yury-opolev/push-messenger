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
    /// Serves the local control channel (the tray's status and actions); `false` turns
    /// it off.
    #[serde(default = "yes")]
    pub control: bool,
    /// The chat peek: the tray may read the owner conversation's last messages, masked,
    /// over the control channel (kept in memory only); `false` turns it off.
    #[serde(default = "yes")]
    pub peek: bool,
    /// Voice calls with the owner; absent, calls are declined.
    #[serde(default)]
    pub calls: Option<CallsConfig>,
}

fn yes() -> bool {
    true
}

/// Voice calls with the owner. Speech (listening, speaking, detecting the end of an
/// utterance) runs in a voice adapter, a separate process (`adapter`); without one the
/// bridge takes no calls and places none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallsConfig {
    /// How a call is answered: `auto` (the default) or `ask`.
    #[serde(default)]
    pub answer: AnswerMode,
    /// The language spoken, a BCP 47 tag: `en-US` by default.
    #[serde(default = "default_language")]
    pub language: String,
    /// The voice adapter, a separate process doing the speech; absent, calls are
    /// declined and not advertised.
    #[serde(default)]
    pub adapter: Option<AdapterConfig>,
    /// When the bridge calls the owner; absent: never.
    #[serde(default)]
    pub notify_by_call: Option<NotifyByCall>,
    /// The removed built-in speech settings: present (whatever their value, `null`
    /// too), the configuration is refused with a pointer to `adapter`.
    #[serde(default, skip_serializing, deserialize_with = "present")]
    stt: Option<Removed>,
    #[serde(default, skip_serializing, deserialize_with = "present")]
    tts: Option<Removed>,
    #[serde(default, skip_serializing, deserialize_with = "present")]
    vad: Option<Removed>,
}

/// A removed field that was written in `config.json`; its value is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Removed;

/// Records that a field is present, whatever its value (`null` included).
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Removed>, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(Some(Removed))
}

/// When the bridge calls its owner: a request unanswered for `after_minutes`, or an
/// urgent background message; at most once per `min_interval_minutes`, never in the
/// `do_not_disturb` hours (local time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyByCall {
    /// Minutes a permission request, question or plan waits before the call; 0 (the
    /// default) never calls for them.
    #[serde(default)]
    pub after_minutes: u32,
    /// Calls for background messages Coda marks urgent.
    #[serde(default)]
    pub urgent: bool,
    /// At most one call per this many minutes: 10 (the default) or more.
    #[serde(default = "default_min_interval")]
    pub min_interval_minutes: u32,
    /// Local time ranges without calls: `"22:00-07:00"`, `"mon-fri 22:00-07:00"`, or a list.
    #[serde(default, deserialize_with = "one_or_many")]
    pub do_not_disturb: Vec<String>,
}

impl Default for NotifyByCall {
    fn default() -> Self {
        Self {
            after_minutes: 0,
            urgent: false,
            min_interval_minutes: default_min_interval(),
            do_not_disturb: Vec::new(),
        }
    }
}

/// The longest `after_minutes`: a card expires after 10 minutes, and the call must ring
/// and talk before then.
pub const NOTIFY_AFTER_MAX: u32 = 8;
/// The shortest `min_interval_minutes`.
pub const NOTIFY_INTERVAL_MIN: u32 = 10;
/// The longest `min_interval_minutes` (a day).
pub const NOTIFY_INTERVAL_MAX: u32 = 1440;

fn default_min_interval() -> u32 {
    NOTIFY_INTERVAL_MIN
}

/// A string or a list of strings.
fn one_or_many<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

impl NotifyByCall {
    /// The policy the core runs; checked by [`CallsConfig::validate`] first.
    pub fn policy(&self) -> Result<crate::voice::policy::NotifyPolicy, String> {
        use std::time::Duration;
        Ok(crate::voice::policy::NotifyPolicy {
            after: (self.after_minutes > 0)
                .then(|| Duration::from_secs(u64::from(self.after_minutes) * 60)),
            urgent: self.urgent,
            min_interval: Duration::from_secs(u64::from(self.min_interval_minutes) * 60),
            quiet: crate::voice::policy::QuietHours::parse(
                &self.do_not_disturb,
                "calls.notify_by_call.do_not_disturb",
            )?,
        })
    }
}

fn default_language() -> String {
    "en-US".to_owned()
}

/// How a call is answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerMode {
    /// At once.
    #[default]
    Auto,
    /// After the owner accepts a card on another device, before the ring ends.
    Ask,
}

/// Where speech runs (docs/voice-adapter-protocol.md §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AdapterConfig {
    /// The bridge starts the adapter and talks to it over its stdin/stdout.
    Spawn(SpawnConfig),
    /// The bridge connects to an adapter already running.
    Connect(ConnectConfig),
}

/// A voice adapter the bridge starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnConfig {
    /// argv: program then arguments; never run through a shell.
    pub command: Vec<String>,
    /// `log` (default: <state dir>/logs/voice-adapter.log) or `discard`.
    #[serde(default)]
    pub stderr: StderrMode,
    /// Extra variables to pass even though they look secret (rarely needed).
    #[serde(default)]
    pub env_allow: Vec<String>,
}

/// Where a spawned adapter's stderr goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StderrMode {
    /// To `<state dir>/logs/voice-adapter.log`.
    #[default]
    Log,
    /// Nowhere.
    Discard,
}

/// A voice adapter already running, listening on a local endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectConfig {
    /// Windows: `\\.\pipe\<name>`; the name only, 1–200 chars of [A-Za-z0-9._-].
    Pipe(String),
    /// Unix: an absolute path after `$XDG_RUNTIME_DIR` expansion.
    Socket(String),
}

/// The longest pipe name in `calls.adapter.connect.pipe`.
pub const PIPE_NAME_MAX: usize = 200;

/// Checks `calls.adapter`; the error names the field.
pub fn validate_adapter(adapter: &AdapterConfig) -> Result<(), String> {
    match adapter {
        AdapterConfig::Spawn(spawn) => {
            let at = "calls.adapter.spawn";
            if spawn.command.is_empty() {
                return Err(format!("{at}.command is empty"));
            }
            if let Some(index) = spawn.command.iter().position(|arg| arg.is_empty()) {
                return Err(format!("{at}.command[{index}] is empty"));
            }
            let program = spawn.command[0].to_ascii_lowercase();
            if program.ends_with(".bat") || program.ends_with(".cmd") {
                return Err(format!(
                    "{at}.command[0] is a batch file; batch files run through cmd.exe — use an \
                     .exe or a script interpreter with the script as an argument"
                ));
            }
            if let Some(index) = spawn
                .env_allow
                .iter()
                .position(|name| !matches(r"^[A-Za-z_][A-Za-z0-9_]*$", name))
            {
                return Err(format!(
                    "{at}.env_allow[{index}] must be an environment variable's name"
                ));
            }
            if let Some(index) = spawn
                .env_allow
                .iter()
                .position(|name| crate::voice::adapter::spawn::is_always_secret(name))
            {
                return Err(format!(
                    "{at}.env_allow[{index}] names a variable the bridge never passes on"
                ));
            }
        }
        AdapterConfig::Connect(ConnectConfig::Pipe(name)) => {
            let at = "calls.adapter.connect.pipe";
            if !cfg!(windows) {
                return Err(format!("{at} is only available on Windows"));
            }
            if name.is_empty()
                || name.len() > PIPE_NAME_MAX
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            {
                return Err(format!(
                    "{at} must be the pipe's name only: 1 to {PIPE_NAME_MAX} of A-Z a-z 0-9 . _ -"
                ));
            }
        }
        AdapterConfig::Connect(ConnectConfig::Socket(path)) => {
            let at = "calls.adapter.connect.socket";
            if !cfg!(unix) {
                return Err(format!("{at} is only available on Unix"));
            }
            if !(path.starts_with('/') || path.starts_with("$XDG_RUNTIME_DIR/")) {
                return Err(format!(
                    "{at} must be an absolute path (it may start with $XDG_RUNTIME_DIR/)"
                ));
            }
        }
    }
    Ok(())
}

/// The speech-to-text provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    from = "ProviderWire<AzureSpeechConfig, LocalSttConfig>",
    into = "ProviderWire<AzureSpeechConfig, LocalSttConfig>"
)]
pub enum SttConfig {
    /// Azure AI Speech's REST API for short audio.
    AzureSpeech(AzureSpeechConfig),
    /// A whisper.cpp-compatible server or command on this machine.
    Local(LocalSttConfig),
    /// No speech-to-text: calls are declined.
    #[default]
    None,
}

/// The text-to-speech provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    from = "ProviderWire<AzureSpeechConfig, LocalTtsConfig>",
    into = "ProviderWire<AzureSpeechConfig, LocalTtsConfig>"
)]
pub enum TtsConfig {
    /// Azure AI Speech's REST API.
    AzureSpeech(AzureSpeechConfig),
    /// A server or command on this machine (such as piper).
    Local(LocalTtsConfig),
    /// No text-to-speech: replies stay in the chat.
    #[default]
    None,
}

/// A provider section as written: `provider` names the variant. `none` holds an empty
/// struct so that its unknown fields are refused too (serde ignores the fields of a
/// unit variant in an internally tagged enum).
#[derive(Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "kebab-case")]
enum ProviderWire<A, L> {
    AzureSpeech(A),
    Local(L),
    None(NoFields),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoFields {}

impl SttConfig {
    /// The provider's name as written in `config.json`.
    pub fn provider(&self) -> &'static str {
        match self {
            Self::AzureSpeech(_) => "azure-speech",
            Self::Local(_) => "local",
            Self::None => "none",
        }
    }
}

impl TtsConfig {
    /// The provider's name as written in `config.json`.
    pub fn provider(&self) -> &'static str {
        match self {
            Self::AzureSpeech(_) => "azure-speech",
            Self::Local(_) => "local",
            Self::None => "none",
        }
    }
}

impl From<ProviderWire<AzureSpeechConfig, LocalSttConfig>> for SttConfig {
    fn from(wire: ProviderWire<AzureSpeechConfig, LocalSttConfig>) -> Self {
        match wire {
            ProviderWire::AzureSpeech(azure) => Self::AzureSpeech(azure),
            ProviderWire::Local(local) => Self::Local(local),
            ProviderWire::None(NoFields {}) => Self::None,
        }
    }
}

impl From<SttConfig> for ProviderWire<AzureSpeechConfig, LocalSttConfig> {
    fn from(config: SttConfig) -> Self {
        match config {
            SttConfig::AzureSpeech(azure) => Self::AzureSpeech(azure),
            SttConfig::Local(local) => Self::Local(local),
            SttConfig::None => Self::None(NoFields {}),
        }
    }
}

impl From<ProviderWire<AzureSpeechConfig, LocalTtsConfig>> for TtsConfig {
    fn from(wire: ProviderWire<AzureSpeechConfig, LocalTtsConfig>) -> Self {
        match wire {
            ProviderWire::AzureSpeech(azure) => Self::AzureSpeech(azure),
            ProviderWire::Local(local) => Self::Local(local),
            ProviderWire::None(NoFields {}) => Self::None,
        }
    }
}

impl From<TtsConfig> for ProviderWire<AzureSpeechConfig, LocalTtsConfig> {
    fn from(config: TtsConfig) -> Self {
        match config {
            TtsConfig::AzureSpeech(azure) => Self::AzureSpeech(azure),
            TtsConfig::Local(local) => Self::Local(local),
            TtsConfig::None => Self::None(NoFields {}),
        }
    }
}

/// Azure AI Speech: a `region` with a key, or a resource's custom domain `endpoint` with
/// a key or Entra sign-in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureSpeechConfig {
    /// The resource's region, such as `westeurope`: the regional endpoints, key auth.
    #[serde(default)]
    pub region: Option<String>,
    /// The resource's custom domain: `https://<name>.cognitiveservices.azure.com`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The environment variable holding the key; `AZURE_SPEECH_KEY` by default.
    #[serde(default)]
    pub key_env: Option<String>,
    /// Signs in with an Entra token instead of a key (needs `endpoint`).
    #[serde(default)]
    pub entra: Option<EntraConfig>,
    /// The voice, for `calls.tts` only: `en-US-JennyNeural` by default.
    #[serde(default)]
    pub voice: Option<String>,
}

/// Entra sign-in for Azure AI Speech.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntraConfig {
    /// The Speech resource's Azure resource ID; when given, the token is sent as
    /// `aad#<resource_id>#<token>`.
    #[serde(default)]
    pub resource_id: Option<String>,
    /// The command printing an access token (run without a shell); by default
    /// `az account get-access-token --resource https://cognitiveservices.azure.com
    /// --query accessToken -o tsv`.
    #[serde(default)]
    pub token_command: Option<Vec<String>>,
}

/// Local speech-to-text: a whisper.cpp server's `/inference` URL, or a command whose
/// arguments name the audio as `{wav}` (and may use `{language}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSttConfig {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

/// Local text-to-speech: a URL answering a text body with a WAV, or a command reading the
/// text on stdin and writing raw 16-bit mono samples at `sample_rate` to stdout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTtsConfig {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// The command's sample rate (required with `command`).
    #[serde(default)]
    pub sample_rate: Option<u32>,
}

impl CallsConfig {
    /// How calls are answered and placed (the section is validated already; a policy
    /// that does not validate is off).
    pub fn call_policy(&self) -> crate::voice::policy::CallPolicy {
        crate::voice::policy::CallPolicy {
            answer: self.answer,
            notify: self
                .notify_by_call
                .as_ref()
                .and_then(|notify| notify.policy().ok())
                .unwrap_or_default(),
            local_time: None,
        }
    }

    /// Checks the section as [`Config::validate`] does; the error names the field.
    pub fn validate(&self) -> Result<(), String> {
        for (field, value) in [("stt", self.stt), ("tts", self.tts), ("vad", self.vad)] {
            if value.is_some() {
                return Err(format!(
                    "calls.{field} is no longer supported: speech runs in a voice adapter. \
                     Configure calls.adapter (docs/agent-bridge.md §18, \
                     docs/voice-adapter-protocol.md)."
                ));
            }
        }
        if !matches(r"^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$", &self.language) {
            return Err("calls.language must be a language tag such as \"en-US\"".to_owned());
        }
        if let Some(adapter) = &self.adapter {
            validate_adapter(adapter)?;
        }
        if let Some(notify) = &self.notify_by_call {
            let at = "calls.notify_by_call";
            if notify.after_minutes > NOTIFY_AFTER_MAX {
                return Err(format!(
                    "{at}.after_minutes must be 0 (off) to {NOTIFY_AFTER_MAX}: a card expires after 10 minutes"
                ));
            }
            if !(NOTIFY_INTERVAL_MIN..=NOTIFY_INTERVAL_MAX).contains(&notify.min_interval_minutes) {
                return Err(format!(
                    "{at}.min_interval_minutes must be {NOTIFY_INTERVAL_MIN} to {NOTIFY_INTERVAL_MAX}"
                ));
            }
            notify.policy()?;
            if self.adapter.is_none() && (notify.after_minutes > 0 || notify.urgent) {
                return Err(format!(
                    "{at} needs calls.adapter: without it the bridge takes no calls and places none"
                ));
            }
        }
        Ok(())
    }
}

fn matches(pattern: &str, text: &str) -> bool {
    regex::Regex::new(pattern)
        .map(|regex| regex.is_match(text))
        .unwrap_or(false)
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
    /// real folder inside the roots), the default permission mode, the calls section
    /// and the backend's section.
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
        if let Some(calls) = &self.calls {
            calls.validate().map_err(ConfigError::Invalid)?;
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
        assert!(config.control);
        assert!(config.peek);
    }

    #[test]
    fn the_chat_peek_can_be_turned_off() {
        let fixture = fixture();
        let mut config = minimal(&fixture);
        config["peek"] = json!(false);
        assert!(!load(&fixture, &config).unwrap().peek);
    }

    #[test]
    fn the_control_channel_can_be_turned_off() {
        let fixture = fixture();
        let mut config = minimal(&fixture);
        config["control"] = json!(false);
        assert!(!load(&fixture, &config).unwrap().control);
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

    fn with_calls(fixture: &Fixture, calls: serde_json::Value) -> Result<Config, ConfigError> {
        let mut config = minimal(fixture);
        config["calls"] = calls;
        load(fixture, &config)
    }

    fn calls_error(fixture: &Fixture, calls: serde_json::Value) -> String {
        let config = with_calls(fixture, calls).unwrap();
        match config.validate() {
            Err(ConfigError::Invalid(message)) => message,
            other => panic!("expected an invalid configuration, got {other:?}"),
        }
    }

    #[test]
    fn calls_are_absent_by_default() {
        let fixture = fixture();
        assert_eq!(load(&fixture, &minimal(&fixture)).unwrap().calls, None);
    }

    const ADAPTER: &str = "push-voice-me";

    fn adapter() -> serde_json::Value {
        json!({ "connect": { "pipe": ADAPTER } })
    }

    fn removed(field: &str) -> String {
        format!(
            "calls.{field} is no longer supported: speech runs in a voice adapter. Configure \
             calls.adapter (docs/agent-bridge.md §18, docs/voice-adapter-protocol.md)."
        )
    }

    #[test]
    fn the_removed_speech_fields_point_to_the_adapter() {
        let fixture = fixture();
        let values = [
            json!(null),
            json!({ "provider": "none" }),
            json!({ "provider": "azure-speech", "region": "westeurope" }),
            json!({ "provider": "local", "url": "http://127.0.0.1:8080/inference" }),
            json!({ "silence_ms": 900 }),
            json!({}),
            json!("anything"),
        ];
        for field in ["stt", "tts", "vad"] {
            for value in &values {
                for with_adapter in [false, true] {
                    let mut calls = json!({});
                    calls[field] = value.clone();
                    if with_adapter {
                        calls["adapter"] = adapter();
                    }
                    let message = calls_error(&fixture, calls.clone());
                    assert_eq!(message, removed(field), "{calls}");
                    assert!(!message.contains('\n'));
                }
            }
        }
    }

    #[test]
    fn the_first_removed_field_is_reported() {
        let fixture = fixture();
        let none = json!({ "provider": "none" });
        let cases = [
            (json!({ "vad": {}, "tts": none, "stt": none }), "stt"),
            (json!({ "vad": {}, "tts": null }), "tts"),
            (json!({ "vad": {}, "language": "de-DE" }), "vad"),
        ];
        for (calls, field) in cases {
            assert_eq!(
                calls_error(&fixture, calls.clone()),
                removed(field),
                "{calls}"
            );
        }
    }

    #[test]
    fn a_removed_field_stops_the_bridge_with_the_message_alone() {
        let fixture = fixture();
        let config = with_calls(&fixture, json!({ "stt": { "provider": "none" } })).unwrap();
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            format!("invalid configuration: {}", removed("stt"))
        );
        assert!(
            !serde_json::to_string(&config).unwrap().contains("stt"),
            "never written back"
        );
    }

    #[test]
    fn a_calls_section_without_an_adapter_loads() {
        let fixture = fixture();
        for calls in [json!({}), json!({ "answer": "ask", "language": "de-DE" })] {
            let config = with_calls(&fixture, calls.clone()).unwrap();
            config.validate().unwrap();
            assert_eq!(config.calls.unwrap().adapter, None, "{calls}");
        }
    }

    #[test]
    fn a_calls_section_parses_with_its_defaults() {
        let fixture = fixture();
        let config = with_calls(&fixture, json!({ "adapter": adapter() })).unwrap();
        config.validate().unwrap();
        let calls = config.calls.unwrap();
        assert_eq!(calls.answer, AnswerMode::Auto);
        assert_eq!(calls.language, "en-US");
        assert_eq!(calls.notify_by_call, None);
        let written = serde_json::to_value(&calls).unwrap();
        assert_eq!(
            serde_json::from_value::<CallsConfig>(written).unwrap(),
            calls
        );
    }

    #[test]
    fn ask_and_notify_by_call_parse_with_their_defaults() {
        let fixture = fixture();
        let config = with_calls(
            &fixture,
            json!({
                "answer": "ask",
                "adapter": adapter(),
                "notify_by_call": { "after_minutes": 5, "do_not_disturb": "22:00-07:00" },
            }),
        )
        .unwrap();
        #[cfg(windows)]
        config.validate().unwrap();
        let calls = config.calls.unwrap();
        assert_eq!(calls.answer, AnswerMode::Ask);
        let notify = calls.notify_by_call.clone().unwrap();
        assert_eq!(
            notify,
            NotifyByCall {
                after_minutes: 5,
                urgent: false,
                min_interval_minutes: 10,
                do_not_disturb: vec!["22:00-07:00".into()],
            }
        );
        let policy = notify.policy().unwrap();
        assert_eq!(policy.after, Some(std::time::Duration::from_secs(300)));
        assert_eq!(policy.min_interval, std::time::Duration::from_secs(600));
        assert!(!policy.urgent);
        let written = serde_json::to_value(&calls).unwrap();
        assert_eq!(
            written["notify_by_call"]["do_not_disturb"],
            json!(["22:00-07:00"])
        );
        assert_eq!(
            serde_json::from_value::<CallsConfig>(written).unwrap(),
            calls
        );
        // Off: no wait and no urgent calls, so no adapter is needed.
        let off = with_calls(
            &fixture,
            json!({
                "notify_by_call": { "do_not_disturb": ["sat,sun 00:00-24:00", "mon-fri 22:00-07:00"], "min_interval_minutes": 1440 },
            }),
        )
        .unwrap();
        off.validate().unwrap();
        let policy = off.calls.unwrap().notify_by_call.unwrap().policy().unwrap();
        assert!(!policy.enabled());
    }

    #[test]
    fn notify_by_call_needs_the_adapter() {
        let fixture = fixture();
        for notify in [json!({ "after_minutes": 5 }), json!({ "urgent": true })] {
            assert_eq!(
                calls_error(&fixture, json!({ "notify_by_call": notify.clone() })),
                "calls.notify_by_call needs calls.adapter: without it the bridge takes no calls and places none",
                "{notify}"
            );
        }
    }

    #[test]
    fn calls_mistakes_are_refused_by_field() {
        let fixture = fixture();
        let spawn = json!({ "spawn": { "command": ["adapter"] } });
        let cases = [
            (json!({ "language": "english!" }), "calls.language"),
            (
                json!({ "adapter": spawn, "notify_by_call": { "after_minutes": 9 } }),
                "calls.notify_by_call.after_minutes must be 0 (off) to 8: a card expires after 10 minutes",
            ),
            (
                json!({ "adapter": spawn, "notify_by_call": { "after_minutes": 5, "min_interval_minutes": 9 } }),
                "calls.notify_by_call.min_interval_minutes must be 10 to 1440",
            ),
            (
                json!({ "adapter": spawn, "notify_by_call": { "urgent": true, "min_interval_minutes": 1441 } }),
                "calls.notify_by_call.min_interval_minutes must be 10 to 1440",
            ),
            (
                json!({ "adapter": spawn, "notify_by_call": { "after_minutes": 5, "do_not_disturb": ["22:00-07:00", "night"] } }),
                "calls.notify_by_call.do_not_disturb[1] needs a time range",
            ),
            (
                json!({ "adapter": spawn, "notify_by_call": { "do_not_disturb": "25:00-07:00" } }),
                "calls.notify_by_call.do_not_disturb[0] has a bad time",
            ),
        ];
        for (calls, expected) in cases {
            let message = calls_error(&fixture, calls.clone());
            assert!(message.contains(expected), "{calls}: {message}");
        }
    }

    #[test]
    fn unknown_calls_fields_are_refused_by_name() {
        let fixture = fixture();
        let cases = [
            (json!({ "anwser": "auto" }), "anwser"),
            (json!({ "speech": { "provider": "none" } }), "speech"),
            (json!({ "notify_by_call": { "after": 5 } }), "after"),
            (json!({ "answer": "maybe" }), "maybe"),
        ];
        for (calls, field) in cases {
            let error = with_calls(&fixture, calls.clone()).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Parse { message, .. } if message.contains(field)),
                "{calls}: {error}"
            );
        }
    }

    #[test]
    fn an_adapter_alone_is_a_calls_section() {
        let fixture = fixture();
        let config = with_calls(
            &fixture,
            json!({ "adapter": { "spawn": { "command": ["C:\\a.exe", "--x"] } } }),
        )
        .unwrap();
        let calls = config.calls.clone().unwrap();
        assert_eq!(
            calls.adapter,
            Some(AdapterConfig::Spawn(SpawnConfig {
                command: vec!["C:\\a.exe".into(), "--x".into()],
                stderr: StderrMode::Log,
                env_allow: Vec::new(),
            }))
        );
        config.validate().unwrap();

        let relative = with_calls(
            &fixture,
            json!({ "adapter": { "spawn": { "command": ["adapter"], "stderr": "discard", "env_allow": ["HF_TOKEN"] } }, "notify_by_call": { "urgent": true } }),
        )
        .unwrap();
        relative.validate().unwrap();

        let pipe = with_calls(
            &fixture,
            json!({ "adapter": { "connect": { "pipe": "push-voice-me" } } }),
        )
        .unwrap();
        assert_eq!(
            pipe.calls.clone().unwrap().adapter,
            Some(AdapterConfig::Connect(ConnectConfig::Pipe(
                "push-voice-me".into()
            )))
        );
        #[cfg(windows)]
        pipe.validate().unwrap();

        #[cfg(unix)]
        for socket in ["/run/push/voice.sock", "$XDG_RUNTIME_DIR/push-voice.sock"] {
            with_calls(
                &fixture,
                json!({ "adapter": { "connect": { "socket": socket } } }),
            )
            .unwrap()
            .validate()
            .unwrap();
        }
    }

    #[test]
    fn adapter_mistakes_are_refused_by_field() {
        let fixture = fixture();
        let long = "a".repeat(201);
        let mut cases = vec![
            (
                json!({ "adapter": { "spawn": { "command": [] } } }),
                "calls.adapter.spawn.command is empty".to_owned(),
            ),
            (
                json!({ "adapter": { "spawn": { "command": ["adapter", ""] } } }),
                "calls.adapter.spawn.command[1] is empty".to_owned(),
            ),
            (
                json!({ "adapter": { "spawn": { "command": ["adapter"], "env_allow": ["A B"] } } }),
                "calls.adapter.spawn.env_allow[0]".to_owned(),
            ),
            (
                json!({ "adapter": { "spawn": { "command": ["adapter"], "env_allow": ["HF_TOKEN", "coda_serve_api_key"] } } }),
                "calls.adapter.spawn.env_allow[1] names a variable the bridge never passes on"
                    .to_owned(),
            ),
            (
                json!({ "adapter": { "spawn": { "command": ["adapter"], "env_allow": ["Push_Agent_Key"] } } }),
                "calls.adapter.spawn.env_allow[0] names a variable the bridge never passes on"
                    .to_owned(),
            ),
            (
                json!({ "adapter": { "spawn": { "command": ["C:\\tools\\adapter.CMD"] } } }),
                "calls.adapter.spawn.command[0] is a batch file".to_owned(),
            ),
            (
                json!({ "adapter": { "spawn": { "command": ["/opt/adapter.bat", "x"] } } }),
                "calls.adapter.spawn.command[0] is a batch file".to_owned(),
            ),
        ];
        #[cfg(windows)]
        {
            for name in ["a\\b", "push voice", "", long.as_str(), "\\\\.\\pipe\\x"] {
                cases.push((
                    json!({ "adapter": { "connect": { "pipe": name } } }),
                    "calls.adapter.connect.pipe must be the pipe's name only".to_owned(),
                ));
            }
            cases.push((
                json!({ "adapter": { "connect": { "socket": "/run/x.sock" } } }),
                "calls.adapter.connect.socket is only available on Unix".to_owned(),
            ));
        }
        #[cfg(unix)]
        {
            let _ = &long;
            cases.push((
                json!({ "adapter": { "connect": { "pipe": "push-voice-me" } } }),
                "calls.adapter.connect.pipe is only available on Windows".to_owned(),
            ));
            for socket in ["voice.sock", "$XDG_RUNTIME_DIR", "$HOME/voice.sock", ""] {
                cases.push((
                    json!({ "adapter": { "connect": { "socket": socket } } }),
                    "calls.adapter.connect.socket must be an absolute path".to_owned(),
                ));
            }
        }
        for (calls, expected) in cases {
            let message = calls_error(&fixture, calls.clone());
            assert!(message.contains(&expected), "{calls}: {message}");
        }
    }

    #[test]
    fn unknown_adapter_fields_are_refused_by_name() {
        let fixture = fixture();
        let cases = [
            (json!({ "spwan": { "command": ["a"] } }), "spwan"),
            (json!({ "spawn": { "command": ["a"], "cwd": "x" } }), "cwd"),
            (
                json!({ "spawn": { "command": ["a"], "stderr": "file" } }),
                "file",
            ),
            (json!({ "spawn": { "args": ["a"] } }), "args"),
            (json!({ "connect": { "tcp": "127.0.0.1:1" } }), "tcp"),
            (json!({ "connect": { "pipe": "x", "socket": "/x" } }), ""),
            (
                json!({ "spawn": { "command": ["a"] }, "connect": { "pipe": "x" } }),
                "",
            ),
        ];
        for (adapter, field) in cases {
            let error = with_calls(&fixture, json!({ "adapter": adapter.clone() })).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Parse { message, .. } if message.contains(field)),
                "{adapter}: {error}"
            );
        }
    }
}
