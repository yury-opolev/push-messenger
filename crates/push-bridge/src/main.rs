//! `push-agent-bridge`: reaches a local AI coding agent (Coda) from push-messenger as a
//! private agent contact. A console program: the logon task runs it in the background.

mod cli;
mod signer;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use push_bridge::coda::CodaBackend;
use push_bridge::config::{self, Config};
use push_bridge::core::{Bridge, BridgeError, BridgeOptions};
use push_bridge::folders;
use push_bridge::logging;
use push_bridge::messenger::{EVENT_QUEUE, SdkMessenger};
use push_bridge::state::{self, BridgeSecretStore, StateFile};
use push_sdk::{Client, Event, Events, Platform, SdkError};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::cli::{Cli, Command, USAGE};
use crate::signer::{KeyError, open_key};

/// A new sign-in after the server signed the agent out waits at least this long after the
/// one before.
const SIGN_IN_INTERVAL: Duration = Duration::from_secs(30);

const ENROLL_HINT: &str = "This agent is not enrolled: run `push-agent-bridge enroll --code <code>` with a code from the agents page.";

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
    println!("Log:           {}", logging::path(dir).display());
    Ok(())
}

/// A client with the in-memory database and the bridge's secret store.
fn open_client(config: &Config, state: &Arc<StateFile>) -> Result<(Client, Events), Failure> {
    let mut sdk = push_sdk::Config::new(&config.api_url, &config.socket_url(), ":memory:");
    sdk.host = config.host.clone();
    sdk.allow_insecure_transport = config.allow_insecure_transport;
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
    let result = loop {
        if let Some(last) = last_sign_in {
            let mut stopping = stopped.clone();
            tokio::select! {
                () = tokio::time::sleep_until(last + SIGN_IN_INTERVAL) => {}
                _ = stopping.wait_for(|stop| *stop) => break Ok(()),
            }
        }
        last_sign_in = Some(Instant::now());
        let mut stopping = stopped.clone();
        let signed_in = tokio::select! {
            signed_in = client.sign_in_with_agent_key(
                Arc::clone(&key.signer),
                Some(&audience),
                &device_name,
                Platform::Desktop,
            ) => signed_in,
            _ = stopping.wait_for(|stop| *stop) => break Ok(()),
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
        let messenger = match SdkMessenger::new(client.clone()) {
            Ok(messenger) => Arc::new(messenger),
            Err(error) => break Err(Failure::general(format!("Cannot start: {error}"))),
        };
        let (backend_sender, backend_events) = mpsc::unbounded_channel();
        let backend = Arc::new(CodaBackend::new(coda.clone(), backend_sender));
        let bridge = Bridge::new(
            messenger,
            backend,
            backend_events,
            messenger_events,
            Arc::clone(&state),
            options.clone(),
        );
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
    if client.close().await.is_err() {
        tracing::warn!(event = "client_close_failed");
    }
    tracing::info!(event = "stopped", ok = result.is_ok());
    result
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
