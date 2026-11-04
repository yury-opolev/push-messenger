//! `push-agent-bridge`: reaches a local AI coding agent (Coda) from push-messenger as a
//! private agent contact. A console program: the logon task runs it in the background.

mod cli;
mod remote;
mod signer;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use push_bridge::coda::CodaBackend;
use push_bridge::config::{self, CallsConfig, Config, SttConfig};
use push_bridge::control::{Action, Calls, ControlCall, ControlHandle, ControlHub, error};
use push_bridge::core::{Bridge, BridgeError, BridgeOptions};
use push_bridge::folders;
use push_bridge::logging;
use push_bridge::messenger::{EVENT_QUEUE, Reconnect, SdkMessenger};
use push_bridge::state::{self, BridgeSecretStore, StateFile};
use push_bridge::voice::VoiceSetup;
use push_bridge_control::transport::Listener;
use push_bridge_control::{
    BridgeState, Call, ErrorBody, Event as ControlEvent, PauseParams, endpoint, instance_id,
};
use push_sdk::{Client, Event, Events, Platform, SdkError};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::cli::{Cli, Command, USAGE};
use crate::remote::RemoteError;
use crate::signer::{KeyError, open_key};

/// A new sign-in after the server signed the agent out waits at least this long after the
/// one before.
const SIGN_IN_INTERVAL: Duration = Duration::from_secs(30);
/// How long the backend may drain when `pause` does not say.
const DEFAULT_DRAIN_SECONDS: u64 = 600;
/// `pause` waits this much longer than the drain for the bridge's answer: longer than
/// the bridge's own grace (120 s), so its answer (even `timeout`) comes first.
const PAUSE_GRACE: Duration = Duration::from_secs(150);
/// How long `resume` waits for the bridge's answer.
const RESUME_TIMEOUT: Duration = Duration::from_secs(30);

const ENROLL_HINT: &str = "This agent is not enrolled: run `push-agent-bridge enroll --code <code>` with a code from the agents page.";

/// Whether this build has voice (feature `voice`: push-sdk's media helper).
const VOICE_BUILT: bool = cfg!(feature = "voice");

/// Why a command failed, and the exit code that says so.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    /// Exit code 1.
    fn general(message: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: message.into(),
        }
    }

    /// Exit code 2: a setup step is missing (no key, not enrolled).
    fn setup(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    /// Exit code 3: the key provider failed. `detail` is its code and message.
    fn signer(detail: &str) -> Self {
        Self {
            code: 3,
            message: format!("The key provider failed: {detail}"),
        }
    }

    fn key(error: KeyError) -> Self {
        match error {
            KeyError::Missing(what) => {
                Self::setup(format!("{what}: run `push-agent-bridge keygen` first."))
            }
            KeyError::Signer(error) => Self::signer(&error.to_string()),
        }
    }

    /// A failed sign-in or enrollment.
    fn sign_in(error: SdkError) -> Self {
        match &error {
            SdkError::Signer(detail) => Self::signer(detail),
            SdkError::Server { code, .. } if code == "token_invalid" => Self::setup(format!(
                "The server does not accept this agent's key ({code}). {ENROLL_HINT}"
            )),
            SdkError::Server { code, .. } if code == "enrollment_invalid" => {
                Self::general("The enrollment code is wrong, expired or already used.")
            }
            _ => Self::general(format!("Sign-in failed: {error}")),
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match cli::parse(std::env::args_os().skip(1), config::default_dir) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("push-agent-bridge: {error}\n\n{USAGE}");
            return ExitCode::from(1);
        }
    };
    let Cli { dir, command } = cli;
    let result = match command {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::Keygen => keygen(&dir),
        Command::Thumbprint => thumbprint(&dir),
        Command::Status => status(&dir),
        Command::Enroll { code } => enroll(&dir, &code).await,
        Command::Run => run(&dir).await,
        Command::Pause {
            reason,
            drain_timeout,
        } => pause(&dir, reason, drain_timeout).await,
        Command::Resume => resume(&dir).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            tracing::error!(event = "exit", code = failure.code);
            eprintln!("push-agent-bridge: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn load_config(dir: &Path, validate: bool) -> Result<Config, Failure> {
    let config =
        Config::load(&dir.join(config::FILE_NAME)).map_err(|e| Failure::general(e.to_string()))?;
    if validate {
        config
            .validate()
            .map_err(|e| Failure::general(e.to_string()))?;
    }
    Ok(config)
}

fn load_state(dir: &Path) -> Result<Arc<StateFile>, Failure> {
    StateFile::load(&dir.join(state::FILE_NAME))
        .map(Arc::new)
        .map_err(|e| Failure::general(e.to_string()))
}

/// The default folder's real path (a relative one is inside the first root).
fn default_folder(config: &Config) -> Option<PathBuf> {
    let first = config.roots.first()?;
    folders::resolve(config.default_folder.to_str()?, first, &config.roots).ok()
}

fn keygen(dir: &Path) -> Result<(), Failure> {
    let config = load_config(dir, false)?;
    let key = open_key(&config.key, dir, true).map_err(Failure::key)?;
    println!("Key storage: {}", key.storage);
    println!("Thumbprint:  {}", key.thumbprint());
    Ok(())
}

fn thumbprint(dir: &Path) -> Result<(), Failure> {
    let config = load_config(dir, false)?;
    let key = open_key(&config.key, dir, false).map_err(Failure::key)?;
    println!("{}", key.thumbprint());
    Ok(())
}

fn status(dir: &Path) -> Result<(), Failure> {
    let config_path = dir.join(config::FILE_NAME);
    println!("Configuration: {}", config_path.display());
    let config = load_config(dir, false)?;
    if let Err(error) = config.validate() {
        println!("               {error}");
    }
    match open_key(&config.key, dir, false) {
        Ok(key) => {
            println!("Key:           {} ({})", config.key.name, key.storage);
            println!("Thumbprint:    {}", key.thumbprint());
        }
        Err(KeyError::Missing(what)) => println!("Key:           none ({what}; run keygen)"),
        Err(KeyError::Signer(error)) => println!("Key:           the key provider failed: {error}"),
    }
    let state = load_state(dir)?.get();
    let or_none = |value: Option<String>| value.unwrap_or_else(|| "none".to_owned());
    println!("Agent user:    {}", or_none(state.enrolled_user_id));
    println!("Device:        {}", or_none(state.device_id));
    let folder = state
        .folder
        .or_else(|| default_folder(&config))
        .unwrap_or_else(|| config.default_folder.clone());
    println!("Folder:        {}", folder.display());
    if state.sessions.is_empty() {
        println!("Sessions:      none");
    } else {
        println!("Sessions:");
        for (folder, session) in &state.sessions {
            println!("  {folder}: {session}");
        }
    }
    println!(
        "Voice:         {}",
        voice_status(config.calls.as_ref(), VOICE_BUILT)
    );
    println!("Log:           {}", logging::path(dir).display());
    Ok(())
}

/// The calls section, when it has someone to listen (an STT provider).
fn listening(calls: Option<&CallsConfig>) -> Option<&CallsConfig> {
    calls.filter(|calls| calls.stt != SttConfig::None)
}

/// Whether the client advertises calls: only a build with voice whose config listens.
/// Otherwise the owner's app shows the agent as unreachable.
fn advertises_calls(calls: Option<&CallsConfig>, voice_built: bool) -> bool {
    voice_built && listening(calls).is_some()
}

/// `status`'s voice line: provider names only, never their settings.
fn voice_status(calls: Option<&CallsConfig>, voice_built: bool) -> String {
    match calls {
        None => "off (no calls section)".to_owned(),
        Some(calls) if calls.stt == SttConfig::None => "off (calls.stt is none)".to_owned(),
        Some(_) if !voice_built => {
            "configured, but this build has no voice (build with --features voice)".to_owned()
        }
        Some(calls) => format!(
            "on: stt {}, tts {}",
            calls.stt.provider(),
            calls.tts.provider()
        ),
    }
}

/// The owner's calls are answered with push-sdk's media helper and the configured
/// speech providers; `None` when the config does not listen.
#[cfg(feature = "voice")]
fn voice_setup(config: &Config, client: &Client) -> Option<VoiceSetup> {
    use push_bridge::voice::media::MediaPhone;
    use push_bridge::voice::provider;

    let calls = listening(config.calls.as_ref())?;
    let providers = provider::build(calls);
    let policy = calls.call_policy();
    tracing::info!(
        event = "voice_enabled",
        stt = providers.stt.as_ref().map_or("none", |stt| stt.name()),
        tts = providers.tts.as_ref().map_or("none", |tts| tts.name()),
        answer = ?calls.answer,
        notify_by_call = policy.notify.enabled(),
    );
    Some(VoiceSetup {
        phone: Arc::new(MediaPhone::new(client)),
        stt: providers.stt,
        tts: providers.tts,
        vad: calls.vad.to_vad(),
        policy,
        adapter: None,
    })
}

/// Without the media helper there is no voice: a calls section only gets a warning.
#[cfg(not(feature = "voice"))]
fn voice_setup(config: &Config, _client: &Client) -> Option<VoiceSetup> {
    if config.calls.is_some() {
        tracing::warn!(event = "voice_unavailable");
        eprintln!(
            "push-agent-bridge: calls is set in config.json, but this build has no voice: build with --features voice"
        );
    }
    None
}

/// The control channel could not be used.
fn remote_failure(dir: &Path, error: RemoteError) -> Failure {
    match error {
        RemoteError::NotRunning => Failure::general(format!(
            "The bridge in {} is not running (no control channel).",
            dir.display()
        )),
        RemoteError::Timeout => Failure::general("The bridge did not answer in time."),
        RemoteError::Broken(reason) => {
            Failure::general(format!("The control channel failed: {reason}"))
        }
    }
}

/// The bridge refused a request.
fn refused(error: ErrorBody) -> Failure {
    Failure::general(format!("{} ({})", error.message, error.code))
}

/// Asks the running bridge to pause its backend for an upgrade and waits until the
/// backend's program is released.
async fn pause(
    dir: &Path,
    reason: Option<String>,
    drain_timeout: Option<u64>,
) -> Result<(), Failure> {
    let mut remote = remote::connect(dir)
        .await
        .map_err(|error| remote_failure(dir, error))?;
    let name = remote
        .backend_name()
        .await
        .unwrap_or_else(|| "The agent".to_owned());
    let params = PauseParams {
        reason,
        drain_timeout_ms: drain_timeout.map(|seconds| seconds.saturating_mul(1000)),
    };
    let result = remote
        .call(Call::PauseBackend(params), pause_limit(drain_timeout))
        .await
        .map_err(|error| remote_failure(dir, error))?;
    result.map_err(refused)?;
    println!("{name} is paused; its program is released.");
    Ok(())
}

/// How long `pause` waits for the bridge's answer: the drain and a grace (however
/// long the drain).
fn pause_limit(drain_timeout: Option<u64>) -> Duration {
    Duration::from_secs(drain_timeout.unwrap_or(DEFAULT_DRAIN_SECONDS)).saturating_add(PAUSE_GRACE)
}

/// Asks the running bridge to start its paused backend again now.
async fn resume(dir: &Path) -> Result<(), Failure> {
    let mut remote = remote::connect(dir)
        .await
        .map_err(|error| remote_failure(dir, error))?;
    let name = remote
        .backend_name()
        .await
        .unwrap_or_else(|| "The agent".to_owned());
    let result = remote
        .call(Call::ResumeBackend, RESUME_TIMEOUT)
        .await
        .map_err(|error| remote_failure(dir, error))?
        .map_err(refused)?;
    if result.get("cancelled") == Some(&json!(true)) {
        println!("The pause was cancelled; {name} keeps running.");
    } else {
        println!("{name} is resuming.");
    }
    Ok(())
}

/// A client with the in-memory database and the bridge's secret store.
fn open_client(config: &Config, state: &Arc<StateFile>) -> Result<(Client, Events), Failure> {
    let mut sdk = push_sdk::Config::new(&config.api_url, &config.socket_url(), ":memory:");
    sdk.host = config.host.clone();
    sdk.allow_insecure_transport = config.allow_insecure_transport;
    sdk.calls = advertises_calls(config.calls.as_ref(), VOICE_BUILT);
    let secrets = Arc::new(BridgeSecretStore::new(Arc::clone(state)));
    Client::open(sdk, secrets).map_err(|e| Failure::general(format!("Cannot start: {e}")))
}

fn start_logging(dir: &Path) -> Result<(), Failure> {
    logging::init(dir)
        .map(drop)
        .map_err(|e| Failure::general(format!("Cannot open the log in {}: {e}", dir.display())))
}

async fn enroll(dir: &Path, code: &str) -> Result<(), Failure> {
    let config = load_config(dir, true)?;
    start_logging(dir)?;
    let key = open_key(&config.key, dir, true).map_err(Failure::key)?;
    let state = load_state(dir)?;
    let (client, _events) = open_client(&config, &state)?;
    let account = client
        .enroll_with_agent_key(
            Arc::clone(&key.signer),
            Some(&config.audience()),
            code,
            &config.device_name(),
            Platform::Desktop,
        )
        .await;
    let _ = client.close().await;
    let account = account.map_err(Failure::sign_in)?;
    state
        .update(|state| state.enrolled_user_id = Some(account.user_id.to_string()))
        .map_err(|e| Failure::general(e.to_string()))?;
    tracing::info!(event = "enrolled");
    println!(
        "Enrolled as {}; device {}. Key thumbprint {}.",
        account.user_id,
        account.device_id,
        key.thumbprint()
    );
    Ok(())
}

async fn run(dir: &Path) -> Result<(), Failure> {
    let config = load_config(dir, true)?;
    start_logging(dir)?;
    let key = open_key(&config.key, dir, false).map_err(Failure::key)?;
    let state = load_state(dir)?;
    if state.get().enrolled_user_id.is_none() {
        return Err(Failure::setup(ENROLL_HINT));
    }
    let hub = config
        .control
        .then(|| ControlHub::new(instance_id(dir), config.peek));
    let serving = hub.as_ref().and_then(serve_control);
    let control = hub.as_ref().map(ControlHub::handle);
    let calls = hub.as_ref().map(ControlHub::calls);
    let coda = config
        .coda
        .clone()
        .ok_or_else(|| Failure::general("backend \"coda\" needs a \"coda\" section"))?;
    let default_folder =
        default_folder(&config).ok_or_else(|| Failure::general("default_folder is not usable"))?;
    let options = BridgeOptions {
        roots: config.roots.clone(),
        default_folder,
        default_mode: config.default_permission_mode,
    };
    let (client, events) = open_client(&config, &state)?;
    let voice = voice_setup(&config, &client);
    let target = forward(events);
    let (stop, stopped) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!(event = "ctrl_c");
            let _ = stop.send(true);
        }
    });
    tracing::info!(event = "started", version = env!("CARGO_PKG_VERSION"));

    let (audience, device_name) = (config.audience(), config.device_name());
    let mut last_sign_in: Option<Instant> = None;
    let result = 'run: loop {
        if let Some(last) = last_sign_in {
            set_state(control.as_ref(), BridgeState::Reconnecting, "offline");
            let mut stopping = stopped.clone();
            loop {
                tokio::select! {
                    () = tokio::time::sleep_until(last + SIGN_IN_INTERVAL) => break,
                    _ = stopping.wait_for(|stop| *stop) => break 'run Ok(()),
                    call = next_call(calls.as_ref()) => match between_runs(call) {
                        Between::Stop => break 'run Ok(()),
                        Between::SignInNow => break,
                        Between::Wait => {}
                    },
                }
            }
        }
        last_sign_in = Some(Instant::now());
        let mut stopping = stopped.clone();
        let sign_in = client.sign_in_with_agent_key(
            Arc::clone(&key.signer),
            Some(&audience),
            &device_name,
            Platform::Desktop,
        );
        tokio::pin!(sign_in);
        let signed_in = loop {
            tokio::select! {
                signed_in = &mut sign_in => break signed_in,
                _ = stopping.wait_for(|stop| *stop) => break 'run Ok(()),
                call = next_call(calls.as_ref()) => {
                    if between_runs(call) == Between::Stop {
                        break 'run Ok(());
                    }
                }
            }
        };
        if let Err(error) = signed_in {
            tracing::warn!(event = "sign_in_failed", code = error.code());
            break Err(Failure::sign_in(error));
        }
        tracing::info!(event = "signed_in");
        // Before connecting, so the new connection's first events reach this loop.
        let (sender, messenger_events) = mpsc::channel(EVENT_QUEUE);
        *target.lock().unwrap_or_else(PoisonError::into_inner) = sender;
        if let Err(error) = client.connect() {
            break Err(Failure::general(format!("Cannot connect: {error}")));
        }
        let reconnect = reconnect(&client, &key.signer, &audience, &device_name);
        let messenger = match SdkMessenger::new(client.clone()) {
            Ok(messenger) => Arc::new(messenger.with_reconnect(reconnect)),
            Err(error) => break Err(Failure::general(format!("Cannot start: {error}"))),
        };
        let (backend_sender, backend_events) = mpsc::unbounded_channel();
        let backend = Arc::new(CodaBackend::new(coda.clone(), backend_sender));
        let mut bridge = Bridge::new(
            messenger,
            backend,
            backend_events,
            messenger_events,
            Arc::clone(&state),
            options.clone(),
        );
        if let (Some(control), Some(calls)) = (&control, &calls) {
            bridge = bridge.with_control(control.clone(), Arc::clone(calls));
        }
        if let Some(voice) = &voice {
            bridge = bridge.with_voice(voice.clone());
        }
        let mut stopping = stopped.clone();
        let shutdown = async move {
            let _ = stopping.wait_for(|stop| *stop).await;
        };
        match bridge.run(shutdown).await {
            Ok(()) => break Ok(()),
            Err(BridgeError::SignedOut) => tracing::warn!(event = "signed_out"),
            Err(error) => break Err(Failure::general(format!("The bridge stopped: {error}"))),
        }
    };
    set_state(control.as_ref(), BridgeState::Stopping, "offline");
    // The phone holds a handle of the client: only the last one closes it.
    drop(voice);
    if client.close().await.is_err() {
        tracing::warn!(event = "client_close_failed");
    }
    if let Some(serving) = serving {
        serving.abort();
    }
    tracing::info!(event = "stopped", ok = result.is_ok());
    result
}

/// Binds the control channel's endpoint and serves it; without it (no endpoint, or
/// another bridge on the same folder) the bridge runs on without a control channel.
fn serve_control(hub: &ControlHub) -> Option<tokio::task::JoinHandle<()>> {
    let id = hub.handle().status().instance;
    let Some(endpoint) = endpoint(&id) else {
        tracing::warn!(event = "control_unavailable", reason = "no_runtime_dir");
        return None;
    };
    match Listener::bind(&endpoint) {
        Ok(listener) => {
            tracing::info!(event = "control_listening", instance = id.as_str());
            Some(hub.serve(listener))
        }
        Err(error) => {
            tracing::warn!(event = "control_unavailable", kind = ?error.kind());
            None
        }
    }
}

/// Sets the state shown on the control channel while no bridge runs.
fn set_state(control: Option<&ControlHandle>, state: BridgeState, connection: &str) {
    let Some(control) = control else {
        return;
    };
    let changed = (control.status().state != Some(state)).then_some(ControlEvent::StateChanged {
        state,
        detail: None,
    });
    control.update_and_emit(
        |status| {
            status.state = Some(state);
            status.detail = None;
            status.connection = Some(connection.to_owned());
        },
        changed,
    );
}

/// Signs the client in again and connects it: the messenger's `reconnect`. A failed
/// sign-in connects anyway, so the old session keeps working when it still can.
fn reconnect(
    client: &Client,
    signer: &Arc<dyn push_sdk::AgentSigner>,
    audience: &str,
    device_name: &str,
) -> Reconnect {
    let (client, signer) = (client.clone(), Arc::clone(signer));
    let (audience, device_name) = (audience.to_owned(), device_name.to_owned());
    Arc::new(move || {
        let (client, signer) = (client.clone(), Arc::clone(&signer));
        let (audience, device_name) = (audience.clone(), device_name.clone());
        Box::pin(async move {
            let signed_in = client
                .sign_in_with_agent_key(signer, Some(&audience), &device_name, Platform::Desktop)
                .await;
            let connected = client.connect();
            signed_in.map(drop)?;
            connected
        })
    })
}

/// The next control action while no bridge runs; never without a control channel.
async fn next_call(calls: Option<&Calls>) -> ControlCall {
    if let Some(calls) = calls {
        if let Some(call) = calls.lock().await.recv().await {
            return call;
        }
    }
    std::future::pending().await
}

/// What the sign-in loop does after a control action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Between {
    Stop,
    SignInNow,
    Wait,
}

/// Answers a control action that came while no bridge runs: `shutdown` stops,
/// `reconnect` signs in now, the others wait for the bridge.
fn between_runs(call: ControlCall) -> Between {
    let (reply, between) = match call.action {
        Action::Shutdown => (Ok(json!({"stopping": true})), Between::Stop),
        Action::Reconnect => (Ok(json!({"reconnected": true})), Between::SignInNow),
        _ => (
            Err(error("unavailable", "the bridge is signing in")),
            Between::Wait,
        ),
    };
    let _ = call.reply.send(reply);
    between
}

/// Forwards the client's events to the bridge loop that currently runs: the returned
/// sender is replaced for each loop. Events between loops are dropped.
fn forward(mut events: Events) -> Arc<Mutex<mpsc::Sender<Event>>> {
    let (closed, _) = mpsc::channel(1);
    let target = Arc::new(Mutex::new(closed));
    let current = Arc::clone(&target);
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            let sender = current
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let _ = sender.send(event).await;
        }
    });
    target
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pause_limit_is_the_drain_and_a_grace_even_for_a_huge_drain() {
        assert_eq!(pause_limit(None), Duration::from_secs(750));
        assert_eq!(pause_limit(Some(30)), Duration::from_secs(180));
        assert_eq!(pause_limit(Some(u64::MAX)), Duration::MAX);
    }

    fn calls(stt: &str, tts: &str) -> CallsConfig {
        let provider = |name: &str| match name {
            "azure-speech" => json!({"provider": name, "region": "westeurope", "key_env": "K"}),
            "local" => json!({"provider": name, "url": "http://127.0.0.1:8080/x"}),
            _ => json!({"provider": name}),
        };
        serde_json::from_value(json!({"stt": provider(stt), "tts": provider(tts)})).unwrap()
    }

    #[test]
    fn calls_are_advertised_only_by_a_voice_build_that_listens() {
        let on = calls("local", "none");
        assert!(advertises_calls(Some(&on), true));
        assert!(!advertises_calls(Some(&on), false));
        assert!(!advertises_calls(Some(&calls("none", "local")), true));
        assert!(!advertises_calls(None, true));
    }

    #[test]
    fn the_voice_line_names_the_providers_only() {
        assert_eq!(voice_status(None, true), "off (no calls section)");
        assert_eq!(
            voice_status(Some(&calls("none", "azure-speech")), true),
            "off (calls.stt is none)"
        );
        assert_eq!(
            voice_status(Some(&calls("azure-speech", "local")), true),
            "on: stt azure-speech, tts local"
        );
        assert_eq!(
            voice_status(Some(&calls("local", "none")), false),
            "configured, but this build has no voice (build with --features voice)"
        );
    }
}
