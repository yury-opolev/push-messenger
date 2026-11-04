//! Speech providers: speech-to-text for the owner's utterances, text-to-speech for the
//! agent's replies. Errors carry content-free codes only: never audio, text, keys or
//! tokens.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::config::{CallsConfig, SttConfig, TtsConfig};

use super::azure::{AzureStt, AzureTts};
use super::local::{LocalStt, LocalTts};

/// How long one request or command may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// A speech provider failed; `code` is content-free.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{provider} failed: {code}")]
pub struct SpeechError {
    pub provider: &'static str,
    pub code: String,
}

impl SpeechError {
    pub fn new(provider: &'static str, code: impl Into<String>) -> Self {
        Self {
            provider,
            code: code.into(),
        }
    }
}

/// Speech-to-text.
#[async_trait::async_trait]
pub trait Stt: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    /// What `samples` (16 kHz mono) say; empty when nothing was understood.
    async fn transcribe(&self, samples: &[i16]) -> Result<String, SpeechError>;
}

/// Text-to-speech.
#[async_trait::async_trait]
pub trait Tts: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    /// `text` spoken, as 16 kHz mono samples.
    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, SpeechError>;
}

/// The configured providers: `stt` `None` means calls are declined; `tts` `None`, replies
/// stay in the chat.
#[derive(Clone)]
pub struct Providers {
    pub stt: Option<Arc<dyn Stt>>,
    pub tts: Option<Arc<dyn Tts>>,
}

/// The providers `calls` configures (validated by `Config::validate`).
pub fn build(calls: &CallsConfig) -> Providers {
    let stt: Option<Arc<dyn Stt>> = match &calls.stt {
        SttConfig::AzureSpeech(azure) => Some(Arc::new(AzureStt::new(azure, &calls.language))),
        SttConfig::Local(local) => match (&local.url, &local.command) {
            (Some(url), _) => Some(Arc::new(LocalStt::http(url, &calls.language))),
            (None, Some(argv)) => Some(Arc::new(LocalStt::command(argv.clone(), &calls.language))),
            (None, None) => None,
        },
        SttConfig::None => None,
    };
    let tts: Option<Arc<dyn Tts>> = match &calls.tts {
        TtsConfig::AzureSpeech(azure) => Some(Arc::new(AzureTts::new(azure, &calls.language))),
        TtsConfig::Local(local) => match (&local.url, &local.command) {
            (Some(url), _) => Some(Arc::new(LocalTts::http(url))),
            (None, Some(argv)) => Some(Arc::new(LocalTts::command(
                argv.clone(),
                local.sample_rate.unwrap_or(super::SAMPLE_RATE),
            ))),
            (None, None) => None,
        },
        TtsConfig::None => None,
    };
    Providers { stt, tts }
}

/// An HTTP client whose requests time out after `timeout`.
pub(crate) fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// A content-free code for a failed request.
pub(crate) fn request_code(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect_failed"
    } else {
        "request_failed"
    }
}

/// Raw signed 16-bit little-endian samples; an odd trailing byte is dropped.
pub(crate) fn samples_from_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// Runs `argv` (never through a shell) with `input` on stdin (or none), stderr
/// discarded, and returns its stdout. Errors: `spawn_failed`, `timeout` (the process is
/// killed), `exit_<code>`, `io`.
///
/// On Windows a program name without an extension is looked up as an `.exe`: a `.cmd` or
/// `.bat` script must be named with its extension.
pub(crate) async fn run_command(
    argv: &[String],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let (program, arguments) = argv
        .split_first()
        .ok_or_else(|| "spawn_failed".to_owned())?;
    let mut child = tokio::process::Command::new(program)
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "spawn_failed".to_owned())?;
    let stdin = child.stdin.take();
    let work = async move {
        let write = async move {
            if let (Some(mut pipe), Some(bytes)) = (stdin, input) {
                // A command that stops reading early is not an error here: its exit
                // status says.
                let _ = pipe.write_all(bytes).await;
                let _ = pipe.shutdown().await;
            }
        };
        let (_, output) = tokio::join!(write, child.wait_with_output());
        output
    };
    // On timeout the future, and with it the child, is dropped: kill_on_drop kills it.
    let output = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| "timeout".to_owned())?
        .map_err(|_| "io".to_owned())?;
    if !output.status.success() {
        return Err(match output.status.code() {
            Some(code) => format!("exit_{code}"),
            None => "exit_signal".to_owned(),
        });
    }
    Ok(output.stdout)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{AzureSpeechConfig, LocalSttConfig, LocalTtsConfig, VadSettings};

    /// `windows` on Windows, else `unix`, as owned arguments.
    pub(crate) fn argv(windows: &[&str], unix: &[&str]) -> Vec<String> {
        let chosen = if cfg!(windows) { windows } else { unix };
        chosen.iter().map(|&argument| argument.to_owned()).collect()
    }

    pub(crate) fn sleeper() -> Vec<String> {
        argv(&["ping", "-n", "6", "127.0.0.1"], &["sleep", "5"])
    }

    #[test]
    fn little_endian_bytes_become_samples() {
        assert_eq!(
            samples_from_le(&[0x01, 0x00, 0xff, 0xff, 0xe8, 0x03, 0x7f]),
            vec![1, -1, 1000]
        );
    }

    #[test]
    fn errors_name_the_provider_and_the_code() {
        assert_eq!(
            SpeechError::new("local", "http_500").to_string(),
            "local failed: http_500"
        );
    }

    #[tokio::test]
    async fn a_command_feeds_stdin_and_returns_stdout() {
        let command = vec!["sort".to_owned()];
        let output = run_command(&command, Some(b"spoken\n"), TIMEOUT)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output).trim(), "spoken");
    }

    #[tokio::test]
    async fn command_failures_have_codes() {
        let missing = vec!["push-bridge-no-such-program".to_owned()];
        assert_eq!(
            run_command(&missing, None, TIMEOUT).await,
            Err("spawn_failed".to_owned())
        );
        let failing = argv(&["cmd", "/C", "exit", "3"], &["sh", "-c", "exit 3"]);
        assert_eq!(
            run_command(&failing, None, TIMEOUT).await,
            Err("exit_3".to_owned())
        );
        let started = std::time::Instant::now();
        assert_eq!(
            run_command(&sleeper(), None, Duration::from_millis(300)).await,
            Err("timeout".to_owned())
        );
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    fn calls(stt: SttConfig, tts: TtsConfig) -> CallsConfig {
        CallsConfig {
            answer: crate::config::AnswerMode::Auto,
            language: "en-US".into(),
            adapter: None,
            stt,
            tts,
            vad: VadSettings::default(),
            notify_by_call: None,
        }
    }

    #[test]
    fn build_makes_the_configured_providers() {
        let azure = AzureSpeechConfig {
            region: Some("westeurope".into()),
            ..AzureSpeechConfig::default()
        };
        let providers = build(&calls(
            SttConfig::AzureSpeech(azure.clone()),
            TtsConfig::AzureSpeech(azure),
        ));
        assert_eq!(providers.stt.unwrap().name(), "azure-speech");
        assert_eq!(providers.tts.unwrap().name(), "azure-speech");

        let providers = build(&calls(
            SttConfig::Local(LocalSttConfig {
                url: Some("http://127.0.0.1:8080/inference".into()),
                command: None,
            }),
            TtsConfig::Local(LocalTtsConfig {
                url: None,
                command: Some(vec!["piper".into()]),
                sample_rate: Some(22050),
            }),
        ));
        assert_eq!(providers.stt.unwrap().name(), "local");
        assert_eq!(providers.tts.unwrap().name(), "local");

        let providers = build(&calls(SttConfig::None, TtsConfig::None));
        assert!(providers.stt.is_none());
        assert!(providers.tts.is_none());
    }
}
