//! `push-voice-adapter-testkit`: the scripted voice adapter as a process.
//!
//! ```text
//! push-voice-adapter-testkit [--script <file>] [--pipe <name> | --socket <path>]
//!                            [--events <path>] [--dump-env]
//! ```
//!
//! - Default (spawn mode): frames on stdin (bridge → adapter) and stdout (adapter →
//!   bridge); stdout carries nothing else, notes go to stderr.
//! - `--pipe <name>` (`\\.\pipe\<name>`) / `--socket <path>` (connect mode): listens as
//!   the bridge's control channel does (private to the user), serves one bridge at a
//!   time, answers another one `error busy`, and serves the next bridge after one leaves.
//! - `--script <file>`: a JSON [`Script`] (default: the default script).
//! - `--events <path>`: appends what it saw and did as JSON lines (content-free).
//! - `--dump-env` (anywhere; every other argument is then ignored, so a test can pass
//!   any): prints its argv (a JSON array) and environment (a JSON object), one line each,
//!   on stdout, and exits 0.
//!
//! Exit codes: 0 when the bridge closed the stream (spawn mode), 1 on an error, 2 on bad
//! arguments, 3 on a scripted crash (`crash_after_frames`).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use push_bridge_control::Endpoint;
use push_bridge_control::transport::{Listener, Stream};
use push_voice_adapter_proto::{
    AdapterMessage, AsyncFrameReader, AsyncFrameWriter, ErrorCode, ErrorMessage,
};
use push_voice_adapter_testkit::{AdapterError, Event, Log, Script, ScriptedAdapter};
use tokio::sync::mpsc;
use tokio::time::timeout;

const USAGE: &str = "usage: push-voice-adapter-testkit [--script <file>] [--pipe <name> | --socket <path>] [--events <path>] [--dump-env]";
const CRASH_EXIT: i32 = 3;

#[derive(Default)]
struct Options {
    script: Option<PathBuf>,
    pipe: Option<String>,
    socket: Option<PathBuf>,
    events: Option<PathBuf>,
}

fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.to_str() {
            Some("--script") => options.script = Some(value("--script")?.into()),
            Some("--pipe") => {
                let name = value("--pipe")?
                    .into_string()
                    .map_err(|_| "--pipe must be UTF-8".to_owned())?;
                options.pipe = Some(name);
            }
            Some("--socket") => options.socket = Some(value("--socket")?.into()),
            Some("--events") => options.events = Some(value("--events")?.into()),
            _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
        }
    }
    if options.pipe.is_some() && options.socket.is_some() {
        return Err("--pipe and --socket cannot both be given".to_owned());
    }
    Ok(options)
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.iter().skip(1).any(|arg| arg == "--dump-env") {
        return dump_env(&args);
    }
    let options = match parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the runtime: {error}");
            return ExitCode::from(1);
        }
    };
    let code = runtime.block_on(run(options));
    // Not a return: tokio's stdin reader may be blocked in a read that would hold the
    // runtime's shutdown.
    std::process::exit(code);
}

fn dump_env(args: &[OsString]) -> ExitCode {
    let argv: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let env: serde_json::Map<String, serde_json::Value> = std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                serde_json::Value::String(value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let mut stdout = std::io::stdout().lock();
    let written = writeln!(stdout, "{}", serde_json::Value::from(argv))
        .and_then(|()| writeln!(stdout, "{}", serde_json::Value::Object(env)))
        .and_then(|()| stdout.flush());
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

async fn run(options: Options) -> i32 {
    let script = match &options.script {
        Some(path) => match Script::load(path) {
            Ok(script) => script,
            Err(error) => {
                eprintln!("{error}");
                return 2;
            }
        },
        None => Script::default(),
    };
    let log = match &options.events {
        Some(path) => match Log::with_file(path) {
            Ok(log) => log,
            Err(error) => {
                eprintln!("cannot open {}: {error}", path.display());
                return 1;
            }
        },
        None => Log::new(),
    };
    let adapter = ScriptedAdapter::with_log(script, log);
    let endpoint = match (options.pipe, options.socket) {
        (Some(name), _) => Endpoint::Pipe(format!(r"\\.\pipe\{name}")),
        (None, Some(path)) => Endpoint::Socket(path),
        (None, None) => return stdio(adapter).await,
    };
    listen(adapter, endpoint).await
}

fn exit_code(result: &Result<push_voice_adapter_testkit::Outcome, AdapterError>) -> i32 {
    match result {
        Ok(_) => 0,
        Err(AdapterError::Crashed) => CRASH_EXIT,
        Err(error) => {
            eprintln!("voice adapter connection failed: {error}");
            1
        }
    }
}

async fn stdio(adapter: ScriptedAdapter) -> i32 {
    let result = adapter.serve(tokio::io::stdin(), tokio::io::stdout()).await;
    exit_code(&result)
}

async fn listen(adapter: ScriptedAdapter, endpoint: Endpoint) -> i32 {
    let mut listener = match Listener::bind(&endpoint) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on {endpoint:?}: {error}");
            return 1;
        }
    };
    eprintln!("listening on {endpoint:?}");
    let adapter = Arc::new(adapter);
    let serving = Arc::new(AtomicBool::new(false));
    let (crashed, mut crash) = mpsc::channel::<()>(1);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok(stream) => stream,
                    Err(error) => {
                        eprintln!("accept failed: {error}");
                        return 1;
                    }
                };
                if serving.swap(true, Ordering::SeqCst) {
                    adapter.log().push(Event::RejectedBusy);
                    tokio::spawn(reject_busy(stream));
                    continue;
                }
                adapter.log().push(Event::Connected);
                let (adapter, serving, crashed) = (adapter.clone(), serving.clone(), crashed.clone());
                tokio::spawn(async move {
                    let (reader, writer) = tokio::io::split(stream);
                    let result = adapter.serve(reader, writer).await;
                    if matches!(result, Err(AdapterError::Crashed)) {
                        let _ = crashed.send(()).await;
                    } else {
                        exit_code(&result);
                    }
                    serving.store(false, Ordering::SeqCst);
                });
            }
            _ = crash.recv() => return CRASH_EXIT,
        }
    }
}

/// A second bridge: its hello is answered `error busy`, then the connection is closed
/// once it leaves (or after a few seconds).
async fn reject_busy(stream: Stream) {
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = AsyncFrameReader::new(reader);
    let mut writer = AsyncFrameWriter::new(writer);
    let _ = timeout(Duration::from_secs(2), reader.read_frame()).await;
    let busy = ErrorMessage::new(ErrorCode::Busy, "serving another bridge");
    let _ = writer.write_message(&AdapterMessage::Error(busy)).await;
    let _ = writer.shutdown().await;
    let _ = timeout(Duration::from_secs(5), async {
        while let Ok(Some(_)) = reader.read_frame().await {}
    })
    .await;
}
