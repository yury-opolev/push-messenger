//! Speech on this machine: a whisper.cpp-compatible speech-to-text server or command, and
//! a text-to-speech server or command (such as piper).
//!
//! Commands run without a shell. On Windows a program name without an extension is
//! looked up as an `.exe`: a `.cmd` or `.bat` script must be named with its extension.

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;

use super::provider::{
    SpeechError, Stt, TIMEOUT, Tts, http_client, request_code, run_command, samples_from_le,
};
use super::{SAMPLE_RATE, wav};

const NAME: &str = "local";

enum SttSource {
    /// A whisper.cpp server's `/inference` URL.
    Http { url: String },
    /// A command whose arguments name the audio as `{wav}`.
    Command { argv: Vec<String> },
}

/// Local speech-to-text.
pub struct LocalStt {
    source: SttSource,
    language: String,
    timeout: Duration,
    http: reqwest::Client,
}

impl LocalStt {
    /// A whisper.cpp-compatible server: the audio is POSTed as multipart form data.
    pub fn http(url: &str, language: &str) -> Self {
        Self::new(
            SttSource::Http {
                url: url.to_owned(),
            },
            language,
        )
    }

    /// A command run with `{wav}` (a temporary WAV file) and `{language}` replaced in its
    /// arguments; its stdout is the transcript.
    pub fn command(argv: Vec<String>, language: &str) -> Self {
        Self::new(SttSource::Command { argv }, language)
    }

    fn new(source: SttSource, language: &str) -> Self {
        Self {
            source,
            language: language.to_owned(),
            timeout: TIMEOUT,
            http: http_client(TIMEOUT),
        }
    }

    /// Another time limit for each request or command (30 s by default).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.http = http_client(timeout);
        self
    }

    /// The language's primary subtag, lowercased: `en` for `en-US`.
    fn primary_language(&self) -> String {
        self.language
            .split('-')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    async fn transcribe_http(&self, url: &str, audio: Vec<u8>) -> Result<String, SpeechError> {
        let boundary = format!("push-bridge-{}", uuid::Uuid::new_v4().simple());
        let body = multipart(
            &boundary,
            &audio,
            &[
                ("response_format", "json"),
                ("temperature", "0"),
                ("language", &self.primary_language()),
            ],
        );
        let response = self
            .http
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| SpeechError::new(NAME, request_code(&error)))?;
        let status = response.status();
        if !status.is_success() {
            return Err(SpeechError::new(NAME, format!("http_{}", status.as_u16())));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| SpeechError::new(NAME, request_code(&error)))?;
        #[derive(Deserialize)]
        struct Inference {
            #[serde(default)]
            text: String,
        }
        let inference: Inference =
            serde_json::from_slice(&bytes).map_err(|_| SpeechError::new(NAME, "bad_response"))?;
        Ok(clean_transcript(&inference.text))
    }

    async fn transcribe_command(
        &self,
        argv: &[String],
        audio: Vec<u8>,
    ) -> Result<String, SpeechError> {
        let file = TempWav::new();
        file.write(&audio)
            .map_err(|_| SpeechError::new(NAME, "io"))?;
        let path = file.0.display().to_string();
        let language = self.primary_language();
        let argv: Vec<String> = argv
            .iter()
            .map(|argument| {
                argument
                    .replace("{wav}", &path)
                    .replace("{language}", &language)
            })
            .collect();
        let output = run_command(&argv, None, self.timeout)
            .await
            .map_err(|code| SpeechError::new(NAME, code))?;
        drop(file);
        Ok(clean_transcript(&String::from_utf8_lossy(&output)))
    }
}

/// A temporary WAV file, removed when dropped (also on errors and timeouts).
struct TempWav(PathBuf);

impl TempWav {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("push-bridge-{}.wav", uuid::Uuid::new_v4())))
    }

    /// Writes `audio` to a new file only this user can read (`/tmp` is shared on Linux).
    fn write(&self, audio: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&self.0)?.write_all(audio)
    }
}

impl Drop for TempWav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A multipart/form-data body: the WAV as `file` (`audio.wav`), then the text fields.
fn multipart(boundary: &str, audio: &[u8], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(audio.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(b"\r\n");
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Bracketed or parenthesised non-speech markers: `[BLANK_AUDIO]`, `[Music]`, `(silence)`.
static MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[^\]]*\]|\([^)]*\)").expect("the marker pattern is valid"));

/// A transcript without non-speech markers, on one line, whitespace collapsed.
fn clean_transcript(text: &str) -> String {
    MARKER
        .replace_all(text, " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait::async_trait]
impl Stt for LocalStt {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn transcribe(&self, samples: &[i16]) -> Result<String, SpeechError> {
        let audio = wav::encode(samples, SAMPLE_RATE);
        match &self.source {
            SttSource::Http { url } => self.transcribe_http(url, audio).await,
            SttSource::Command { argv } => self.transcribe_command(argv, audio).await,
        }
    }
}

enum TtsSource {
    /// Answers the text with a WAV file.
    Http { url: String },
    /// Reads the text on stdin, writes raw s16le mono at `sample_rate` to stdout.
    Command { argv: Vec<String>, sample_rate: u32 },
}

/// Local text-to-speech.
pub struct LocalTts {
    source: TtsSource,
    timeout: Duration,
    http: reqwest::Client,
}

impl LocalTts {
    /// A server answering a `text/plain` body with a WAV file.
    pub fn http(url: &str) -> Self {
        Self::new(TtsSource::Http {
            url: url.to_owned(),
        })
    }

    /// A command reading the text on stdin and writing raw 16-bit little-endian mono
    /// samples at `sample_rate` to stdout (`piper --model … --output_raw`).
    pub fn command(argv: Vec<String>, sample_rate: u32) -> Self {
        Self::new(TtsSource::Command { argv, sample_rate })
    }

    fn new(source: TtsSource) -> Self {
        Self {
            source,
            timeout: TIMEOUT,
            http: http_client(TIMEOUT),
        }
    }

    /// Another time limit for each request or command (30 s by default).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.http = http_client(timeout);
        self
    }
}

#[async_trait::async_trait]
impl Tts for LocalTts {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, SpeechError> {
        match &self.source {
            TtsSource::Http { url } => {
                let response = self
                    .http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(text.to_owned())
                    .send()
                    .await
                    .map_err(|error| SpeechError::new(NAME, request_code(&error)))?;
                let status = response.status();
                if !status.is_success() {
                    return Err(SpeechError::new(NAME, format!("http_{}", status.as_u16())));
                }
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| SpeechError::new(NAME, request_code(&error)))?;
                let (rate, samples) =
                    wav::decode(&bytes).map_err(|_| SpeechError::new(NAME, "bad_audio"))?;
                Ok(wav::resample(&samples, rate, SAMPLE_RATE))
            }
            TtsSource::Command { argv, sample_rate } => {
                let output = run_command(argv, Some(text.as_bytes()), self.timeout)
                    .await
                    .map_err(|code| SpeechError::new(NAME, code))?;
                Ok(wav::resample(
                    &samples_from_le(&output),
                    *sample_rate,
                    SAMPLE_RATE,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::voice::provider::tests::{argv, sleeper};
    use crate::voice::test_server::{Canned, TestServer};

    #[test]
    fn transcripts_lose_non_speech_markers() {
        assert_eq!(clean_transcript(" [BLANK_AUDIO]\n"), "");
        assert_eq!(clean_transcript("[Music] (silence) [ Silence ]"), "");
        assert_eq!(
            clean_transcript(" Run the\n tests  [Music]\r\n please. "),
            "Run the tests please."
        );
    }

    #[tokio::test]
    async fn whisper_server_gets_a_multipart_wav() {
        let server =
            TestServer::start(vec![Canned::json(json!({ "text": " Hello\n there. " }))]).await;
        let samples: Vec<i16> = (0..800).map(|i| (i * 7 % 300) as i16).collect();
        let stt = LocalStt::http(&format!("{}/inference", server.url), "en-US");
        assert_eq!(stt.transcribe(&samples).await.unwrap(), "Hello there.");
        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/inference");
        let content_type = request.header("content-type").unwrap();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let audio = wav::encode(&samples, 16_000);
        assert_eq!(
            request.body,
            multipart(
                boundary,
                &audio,
                &[
                    ("response_format", "json"),
                    ("temperature", "0"),
                    ("language", "en"),
                ]
            )
        );
        let body = String::from_utf8_lossy(&request.body);
        assert!(body.contains(
            "Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF"
        ));
        assert!(body.contains("name=\"language\"\r\n\r\nen\r\n"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[tokio::test]
    async fn whisper_server_blank_audio_and_failures() {
        let server =
            TestServer::start(vec![Canned::json(json!({ "text": " [BLANK_AUDIO]" }))]).await;
        let stt = LocalStt::http(&server.url, "en-US");
        assert_eq!(stt.transcribe(&[0; 320]).await.unwrap(), "");
        let server = TestServer::start(vec![Canned::new(500, "text/plain", "")]).await;
        let stt = LocalStt::http(&server.url, "en-US");
        assert_eq!(
            stt.transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "http_500"))
        );
    }

    #[tokio::test]
    async fn a_command_transcribes_a_temporary_wav_that_is_removed() {
        let command = argv(
            &[
                "cmd",
                "/C",
                "if",
                "exist",
                "{wav}",
                "echo",
                "{wav}",
                "{language}",
            ],
            &[
                "sh",
                "-c",
                "test -f \"$0\" && echo \"$0\" \"$1\"",
                "{wav}",
                "{language}",
            ],
        );
        let stt = LocalStt::command(command, "de-DE");
        let text = stt.transcribe(&[100; 320]).await.unwrap();
        let (path, language) = text.rsplit_once(' ').unwrap();
        assert_eq!(language, "de");
        let path = std::path::Path::new(path);
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("push-bridge-"),
            "{text}"
        );
        assert!(!path.exists(), "the temporary WAV is removed");
    }

    #[tokio::test]
    async fn a_command_transcript_is_cleaned() {
        let command = argv(
            &[
                "cmd",
                "/C",
                "echo",
                "[BLANK_AUDIO]&",
                "echo",
                "hello&",
                "echo",
                "there",
            ],
            &["sh", "-c", "printf ' [BLANK_AUDIO]\\nhello\\n there\\n'"],
        );
        let stt = LocalStt::command(command, "en-US");
        assert_eq!(stt.transcribe(&[0; 320]).await.unwrap(), "hello there");
    }

    /// A command that writes the WAV's path to `note`, then runs `windows_then` (cmd's
    /// arguments) or `unix_then` (a shell snippet).
    fn noting(note: &std::path::Path, windows_then: &[&str], unix_then: &str) -> Vec<String> {
        let note = note.display().to_string();
        let marker = format!("{{wav}}>{note}&");
        let mut windows = vec!["cmd", "/C", "echo", marker.as_str()];
        windows.extend_from_slice(windows_then);
        argv(
            &windows,
            &[
                "sh",
                "-c",
                &format!("echo \"$0\" > \"$1\"; {unix_then}"),
                "{wav}",
                &note,
            ],
        )
    }

    fn noted_wav(note: &std::path::Path) -> PathBuf {
        PathBuf::from(std::fs::read_to_string(note).unwrap().trim())
    }

    #[tokio::test]
    async fn command_failures_have_codes_and_remove_the_file() {
        let dir = crate::test_dir::TestDir::new();
        let note = dir.path().join("failing.txt");
        let failing = noting(&note, &["exit", "4"], "exit 4");
        assert_eq!(
            LocalStt::command(failing, "en").transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "exit_4"))
        );
        let wav = noted_wav(&note);
        assert!(
            wav.to_string_lossy().contains("push-bridge-"),
            "{}",
            wav.display()
        );
        assert!(
            !wav.exists(),
            "the temporary WAV is removed after a failure"
        );

        let note = dir.path().join("slow.txt");
        let slow = noting(&note, &["ping", "-n", "6", "127.0.0.1"], "sleep 5");
        let stt = LocalStt::command(slow, "en").with_timeout(Duration::from_millis(1500));
        assert_eq!(
            stt.transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "timeout"))
        );
        let wav = noted_wav(&note);
        assert!(
            !wav.exists(),
            "the temporary WAV is removed after a timeout"
        );
    }

    #[tokio::test]
    async fn tts_server_wav_is_resampled_to_16_khz() {
        let samples: Vec<i16> = (0..22_050).map(|i| ((i % 50) * 100) as i16).collect();
        let server = TestServer::start(vec![Canned::new(
            200,
            "audio/wav",
            wav::encode(&samples, 22_050),
        )])
        .await;
        let tts = LocalTts::http(&format!("{}/tts", server.url));
        let spoken = tts.synthesize("Hello <there> & ünïcode").await.unwrap();
        assert_eq!(spoken.len(), 16_000);
        let request = &server.requests()[0];
        assert_eq!(request.path, "/tts");
        assert_eq!(
            request.header("content-type"),
            Some("text/plain; charset=utf-8")
        );
        assert_eq!(request.body, "Hello <there> & ünïcode".as_bytes());
    }

    #[tokio::test]
    async fn tts_server_bad_audio_and_failures() {
        let server = TestServer::start(vec![Canned::new(200, "audio/wav", "not a wav")]).await;
        assert_eq!(
            LocalTts::http(&server.url).synthesize("x").await,
            Err(SpeechError::new(NAME, "bad_audio"))
        );
        let server = TestServer::start(vec![Canned::new(404, "text/plain", "")]).await;
        assert_eq!(
            LocalTts::http(&server.url).synthesize("x").await,
            Err(SpeechError::new(NAME, "http_404"))
        );
    }

    #[tokio::test]
    async fn a_tts_command_reads_the_text_and_writes_raw_samples() {
        // 80 samples of 1000 per byte of text, at 8 kHz.
        let command = argv(
            &[
                "powershell",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$t = [Console]::In.ReadToEnd(); $n = $t.Length * 80; $b = New-Object byte[] ($n * 2); for ($i = 0; $i -lt $n; $i++) { $b[2 * $i] = 0xE8; $b[2 * $i + 1] = 3 }; $o = [Console]::OpenStandardOutput(); $o.Write($b, 0, $b.Length); $o.Flush()",
            ],
            &[
                "sh",
                "-c",
                "n=$(wc -c); i=0; while [ $i -lt $((n * 80)) ]; do printf '\\350\\003'; i=$((i + 1)); done",
            ],
        );
        let tts = LocalTts::command(command, 8_000);
        let spoken = tts.synthesize("hello").await.unwrap();
        assert_eq!(spoken.len(), 800);
        assert!(
            spoken[100..700]
                .iter()
                .all(|&sample| (990..=1010).contains(&sample))
        );
    }

    #[tokio::test]
    async fn tts_command_failures_have_codes() {
        let failing = argv(&["cmd", "/C", "exit", "5"], &["sh", "-c", "exit 5"]);
        assert_eq!(
            LocalTts::command(failing, 16_000).synthesize("x").await,
            Err(SpeechError::new(NAME, "exit_5"))
        );
        let tts = LocalTts::command(sleeper(), 16_000).with_timeout(Duration::from_millis(300));
        assert_eq!(
            tts.synthesize("x").await,
            Err(SpeechError::new(NAME, "timeout"))
        );
    }
}
