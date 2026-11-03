//! Azure AI Speech over REST: speech-to-text for short audio and text-to-speech, with a
//! key (`Ocp-Apim-Subscription-Key`) or an Entra token (a custom domain endpoint).

use std::time::{Duration, Instant};

use reqwest::RequestBuilder;
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;

use crate::config::AzureSpeechConfig;

use super::provider::{SpeechError, Stt, TIMEOUT, Tts, http_client, request_code, run_command};
use super::{SAMPLE_RATE, wav};

const NAME: &str = "azure-speech";
const DEFAULT_KEY_ENV: &str = "AZURE_SPEECH_KEY";
const DEFAULT_VOICE: &str = "en-US-JennyNeural";
/// How long an Entra token is reused (they live about an hour).
const TOKEN_TTL: Duration = Duration::from_secs(30 * 60);
const KEY_HEADER: &str = "ocp-apim-subscription-key";

/// The token command when none is configured: the Azure CLI.
fn default_token_command() -> Vec<String> {
    let az = if cfg!(windows) { "az.cmd" } else { "az" };
    [
        az,
        "account",
        "get-access-token",
        "--resource",
        "https://cognitiveservices.azure.com",
        "--query",
        "accessToken",
        "-o",
        "tsv",
    ]
    .iter()
    .map(|&argument| argument.to_owned())
    .collect()
}

/// The speech-to-text URL for `config`.
pub fn stt_url(config: &AzureSpeechConfig) -> String {
    const PATH: &str = "speech/recognition/conversation/cognitiveservices/v1";
    match (&config.region, &config.endpoint) {
        (Some(region), _) => format!("https://{region}.stt.speech.microsoft.com/{PATH}"),
        (None, Some(endpoint)) => format!("{}/stt/{PATH}", endpoint.trim_end_matches('/')),
        (None, None) => String::new(),
    }
}

/// The text-to-speech URL for `config`.
pub fn tts_url(config: &AzureSpeechConfig) -> String {
    match (&config.region, &config.endpoint) {
        (Some(region), _) => {
            format!("https://{region}.tts.speech.microsoft.com/cognitiveservices/v1")
        }
        (None, Some(endpoint)) => format!(
            "{}/tts/cognitiveservices/v1",
            endpoint.trim_end_matches('/')
        ),
        (None, None) => String::new(),
    }
}

/// Reads an environment variable (replaced in tests).
type Lookup = fn(&str) -> Option<String>;

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

enum Auth {
    /// The key is read from the variable at each request.
    Key {
        env: String,
        lookup: Lookup,
    },
    Entra(Entra),
}

/// Entra tokens from a command, cached for `ttl`.
struct Entra {
    resource_id: Option<String>,
    command: Vec<String>,
    ttl: Duration,
    timeout: Duration,
    cached: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl Entra {
    async fn token(&self) -> Result<String, String> {
        let mut cached = self.cached.lock().await;
        if let Some((token, at)) = cached.as_ref() {
            if at.elapsed() < self.ttl {
                return Ok(token.clone());
            }
        }
        let output = run_command(&self.command, None, self.timeout)
            .await
            .map_err(|code| format!("token_{code}"))?;
        let token = String::from_utf8_lossy(&output).trim().to_owned();
        if token.is_empty() {
            return Err("no_token".to_owned());
        }
        *cached = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    async fn forget(&self) {
        *self.cached.lock().await = None;
    }
}

/// One Azure Speech endpoint with its authentication.
struct Service {
    url: String,
    auth: Auth,
    http: reqwest::Client,
}

impl Service {
    fn new(config: &AzureSpeechConfig, url: String) -> Self {
        let auth = match &config.entra {
            Some(entra) => Auth::Entra(Entra {
                resource_id: entra.resource_id.clone(),
                command: entra
                    .token_command
                    .clone()
                    .unwrap_or_else(default_token_command),
                ttl: TOKEN_TTL,
                timeout: TIMEOUT,
                cached: tokio::sync::Mutex::new(None),
            }),
            None => Auth::Key {
                env: config
                    .key_env
                    .clone()
                    .unwrap_or_else(|| DEFAULT_KEY_ENV.to_owned()),
                lookup: env_lookup,
            },
        };
        Self {
            url,
            auth,
            http: http_client(TIMEOUT),
        }
    }

    async fn header(&self) -> Result<(HeaderName, HeaderValue), String> {
        let (name, value) = match &self.auth {
            Auth::Key { env, lookup } => {
                let key = lookup(env)
                    .map(|key| key.trim().to_owned())
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| "no_key".to_owned())?;
                (HeaderName::from_static(KEY_HEADER), key)
            }
            Auth::Entra(entra) => {
                let token = entra.token().await?;
                let value = match &entra.resource_id {
                    Some(resource_id) => format!("Bearer aad#{resource_id}#{token}"),
                    None => format!("Bearer {token}"),
                };
                (reqwest::header::AUTHORIZATION, value)
            }
        };
        let mut value = HeaderValue::from_str(&value).map_err(|_| "bad_credential".to_owned())?;
        value.set_sensitive(true);
        Ok((name, value))
    }

    /// POSTs the request `build` makes and returns the body of a 2xx answer. With Entra,
    /// a 401 drops the cached token and tries once more with a fresh one.
    async fn post(
        &self,
        build: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Vec<u8>, SpeechError> {
        let fail = |code: String| SpeechError::new(NAME, code);
        let retries = if matches!(self.auth, Auth::Entra(_)) {
            1
        } else {
            0
        };
        let mut attempt = 0;
        loop {
            let (name, value) = self.header().await.map_err(fail)?;
            let response = build(self.http.post(&self.url))
                .header(name, value)
                .send()
                .await
                .map_err(|error| fail(request_code(&error).to_owned()))?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt < retries {
                if let Auth::Entra(entra) = &self.auth {
                    entra.forget().await;
                }
                attempt += 1;
                continue;
            }
            if !status.is_success() {
                return Err(fail(format!("http_{}", status.as_u16())));
            }
            let body = response
                .bytes()
                .await
                .map_err(|error| fail(request_code(&error).to_owned()))?;
            return Ok(body.to_vec());
        }
    }
}

/// Azure AI Speech speech-to-text (REST for short audio, up to 60 s).
pub struct AzureStt {
    service: Service,
    language: String,
}

impl AzureStt {
    pub fn new(config: &AzureSpeechConfig, language: &str) -> Self {
        Self {
            service: Service::new(config, stt_url(config)),
            language: language.to_owned(),
        }
    }

    /// Sends the requests to `url` instead.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.service.url = url.into();
        self
    }
}

#[derive(Deserialize)]
struct Recognition {
    #[serde(rename = "RecognitionStatus")]
    status: String,
    #[serde(rename = "DisplayText", default)]
    text: String,
}

#[async_trait::async_trait]
impl Stt for AzureStt {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn transcribe(&self, samples: &[i16]) -> Result<String, SpeechError> {
        let audio = wav::encode(samples, SAMPLE_RATE);
        let body = self
            .service
            .post(|request| {
                request
                    .query(&[("language", self.language.as_str()), ("format", "simple")])
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "audio/wav; codecs=audio/pcm; samplerate=16000",
                    )
                    .header(reqwest::header::ACCEPT, "application/json")
                    .body(audio.clone())
            })
            .await?;
        let recognition: Recognition =
            serde_json::from_slice(&body).map_err(|_| SpeechError::new(NAME, "bad_response"))?;
        match recognition.status.as_str() {
            "Success" => Ok(recognition.text.trim().to_owned()),
            "NoMatch" | "InitialSilenceTimeout" | "BabbleTimeout" => Ok(String::new()),
            other => Err(SpeechError::new(NAME, status_code(other))),
        }
    }
}

/// A recognition status as an error code: letters and digits only, at most 40.
fn status_code(status: &str) -> String {
    let code: String = status
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(40)
        .collect();
    if code.is_empty() {
        "bad_response".to_owned()
    } else {
        code
    }
}

/// Azure AI Speech text-to-speech: 16 kHz 16-bit mono PCM.
pub struct AzureTts {
    service: Service,
    language: String,
    voice: String,
}

impl AzureTts {
    pub fn new(config: &AzureSpeechConfig, language: &str) -> Self {
        Self {
            service: Service::new(config, tts_url(config)),
            language: language.to_owned(),
            voice: config
                .voice
                .clone()
                .unwrap_or_else(|| DEFAULT_VOICE.to_owned()),
        }
    }

    /// Sends the requests to `url` instead.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.service.url = url.into();
        self
    }

    fn ssml(&self, text: &str) -> String {
        format!(
            "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xml:lang=\"{}\"><voice name=\"{}\">{}</voice></speak>",
            xml_escape(&self.language),
            xml_escape(&self.voice),
            xml_escape(text)
        )
    }
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[async_trait::async_trait]
impl Tts for AzureTts {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, SpeechError> {
        let ssml = self.ssml(text);
        let body = self
            .service
            .post(|request| {
                request
                    .header(reqwest::header::CONTENT_TYPE, "application/ssml+xml")
                    .header("X-Microsoft-OutputFormat", "raw-16khz-16bit-mono-pcm")
                    .header(reqwest::header::USER_AGENT, "push-agent-bridge")
                    .body(ssml.clone())
            })
            .await?;
        Ok(super::provider::samples_from_le(&body))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::EntraConfig;
    use crate::test_dir::TestDir;
    use crate::voice::provider::tests::argv;
    use crate::voice::test_server::{Canned, TestServer};

    fn test_key(name: &str) -> Option<String> {
        (name == "TEST_SPEECH_KEY").then(|| "key-abc".to_owned())
    }

    fn keyed() -> AzureSpeechConfig {
        AzureSpeechConfig {
            region: Some("westeurope".into()),
            key_env: Some("TEST_SPEECH_KEY".into()),
            ..AzureSpeechConfig::default()
        }
    }

    fn with_test_key(mut service: Service) -> Service {
        if let Auth::Key { lookup, .. } = &mut service.auth {
            *lookup = test_key;
        }
        service
    }

    fn keyed_stt(server: &TestServer) -> AzureStt {
        let mut stt = AzureStt::new(&keyed(), "en-US").with_url(format!("{}/stt", server.url));
        stt.service = with_test_key(stt.service);
        stt
    }

    fn keyed_tts(server: &TestServer, voice: Option<&str>) -> AzureTts {
        let config = AzureSpeechConfig {
            voice: voice.map(str::to_owned),
            ..keyed()
        };
        let mut tts = AzureTts::new(&config, "en-US").with_url(format!("{}/tts", server.url));
        tts.service = with_test_key(tts.service);
        tts
    }

    fn token_command() -> Vec<String> {
        argv(
            &["cmd", "/C", "echo", "tok-123"],
            &["sh", "-c", "echo tok-123"],
        )
    }

    /// A token command that also appends a line to `count` each time it runs.
    fn counting_token_command(count: &std::path::Path) -> Vec<String> {
        let path = count.display().to_string();
        if cfg!(windows) {
            argv(
                &[
                    "cmd",
                    "/C",
                    "echo",
                    &format!("x>>{path}&"),
                    "echo",
                    "tok-123",
                ],
                &[],
            )
        } else {
            argv(&[], &["sh", "-c", "echo x >> \"$0\"; echo tok-123", &path])
        }
    }

    fn runs(count: &std::path::Path) -> usize {
        std::fs::read_to_string(count)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn entra(resource_id: Option<&str>, command: Vec<String>) -> AzureSpeechConfig {
        AzureSpeechConfig {
            endpoint: Some("https://my-speech.cognitiveservices.azure.com/".into()),
            entra: Some(EntraConfig {
                resource_id: resource_id.map(str::to_owned),
                token_command: Some(command),
            }),
            ..AzureSpeechConfig::default()
        }
    }

    fn success(text: &str) -> Canned {
        Canned::json(json!({ "RecognitionStatus": "Success", "DisplayText": text, "Offset": 0 }))
    }

    #[test]
    fn urls_follow_the_region_or_the_endpoint() {
        assert_eq!(
            stt_url(&keyed()),
            "https://westeurope.stt.speech.microsoft.com/speech/recognition/conversation/cognitiveservices/v1"
        );
        assert_eq!(
            tts_url(&keyed()),
            "https://westeurope.tts.speech.microsoft.com/cognitiveservices/v1"
        );
        let custom = entra(None, token_command());
        assert_eq!(
            stt_url(&custom),
            "https://my-speech.cognitiveservices.azure.com/stt/speech/recognition/conversation/cognitiveservices/v1"
        );
        assert_eq!(
            tts_url(&custom),
            "https://my-speech.cognitiveservices.azure.com/tts/cognitiveservices/v1"
        );
    }

    #[tokio::test]
    async fn speech_to_text_posts_a_wav_with_the_key() {
        let server = TestServer::start(vec![success("  Hello there.  ")]).await;
        let samples: Vec<i16> = (0..1600).map(|i| (i % 100) as i16).collect();
        let text = keyed_stt(&server).transcribe(&samples).await.unwrap();
        assert_eq!(text, "Hello there.");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/stt");
        assert_eq!(request.query, "language=en-US&format=simple");
        assert_eq!(request.header(KEY_HEADER), Some("key-abc"));
        assert_eq!(request.header("authorization"), None);
        assert_eq!(
            request.header("content-type"),
            Some("audio/wav; codecs=audio/pcm; samplerate=16000")
        );
        assert_eq!(request.header("accept"), Some("application/json"));
        assert_eq!(request.body, wav::encode(&samples, 16_000));
    }

    #[tokio::test]
    async fn recognition_statuses_map_to_text_or_codes() {
        for status in ["NoMatch", "InitialSilenceTimeout", "BabbleTimeout"] {
            let server =
                TestServer::start(vec![Canned::json(json!({ "RecognitionStatus": status }))]).await;
            assert_eq!(
                keyed_stt(&server).transcribe(&[0; 320]).await,
                Ok(String::new())
            );
        }
        let server =
            TestServer::start(vec![Canned::json(json!({ "RecognitionStatus": "Error" }))]).await;
        assert_eq!(
            keyed_stt(&server).transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "Error"))
        );
        let server = TestServer::start(vec![Canned::new(500, "text/plain", "secret detail")]).await;
        assert_eq!(
            keyed_stt(&server).transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "http_500"))
        );
        let server = TestServer::start(vec![Canned::new(200, "text/plain", "not json")]).await;
        assert_eq!(
            keyed_stt(&server).transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "bad_response"))
        );
    }

    #[tokio::test]
    async fn a_missing_key_is_no_key() {
        let server = TestServer::start(vec![success("hi")]).await;
        let config = AzureSpeechConfig {
            key_env: Some("PUSH_BRIDGE_TEST_NO_SUCH_SPEECH_KEY".into()),
            ..keyed()
        };
        let stt = AzureStt::new(&config, "en-US").with_url(&server.url);
        assert_eq!(
            stt.transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "no_key"))
        );
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn a_key_is_not_retried_on_401() {
        let server = TestServer::start(vec![Canned::new(401, "text/plain", "")]).await;
        assert_eq!(
            keyed_stt(&server).transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "http_401"))
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn entra_sends_a_bearer_token() {
        let server = TestServer::start(vec![success("hi")]).await;
        let stt = AzureStt::new(&entra(None, token_command()), "en-US").with_url(&server.url);
        assert_eq!(stt.transcribe(&[0; 320]).await.unwrap(), "hi");
        let requests = server.requests();
        assert_eq!(requests[0].header("authorization"), Some("Bearer tok-123"));
        assert_eq!(requests[0].header(KEY_HEADER), None);

        let resource =
            "/subscriptions/s/resourceGroups/g/providers/Microsoft.CognitiveServices/accounts/a";
        let server = TestServer::start(vec![success("hi")]).await;
        let stt =
            AzureStt::new(&entra(Some(resource), token_command()), "en-US").with_url(&server.url);
        stt.transcribe(&[0; 320]).await.unwrap();
        assert_eq!(
            server.requests()[0].header("authorization"),
            Some(format!("Bearer aad#{resource}#tok-123").as_str())
        );
    }

    #[tokio::test]
    async fn the_token_is_cached_and_refreshed_after_a_401() {
        let dir = TestDir::new();
        let count = dir.path().join("count.txt");
        let server = TestServer::start(vec![
            success("one"),
            success("two"),
            Canned::new(401, "text/plain", ""),
            success("three"),
        ])
        .await;
        let stt = AzureStt::new(&entra(None, counting_token_command(&count)), "en-US")
            .with_url(&server.url);
        assert_eq!(stt.transcribe(&[0; 320]).await.unwrap(), "one");
        assert_eq!(stt.transcribe(&[0; 320]).await.unwrap(), "two");
        assert_eq!(runs(&count), 1, "the token is reused");
        assert_eq!(stt.transcribe(&[0; 320]).await.unwrap(), "three");
        assert_eq!(runs(&count), 2, "a 401 fetches a fresh token");
        assert_eq!(server.requests().len(), 4);
    }

    #[tokio::test]
    async fn an_old_token_is_fetched_again() {
        let dir = TestDir::new();
        let count = dir.path().join("count.txt");
        let server = TestServer::start(vec![success("hi")]).await;
        let mut stt = AzureStt::new(&entra(None, counting_token_command(&count)), "en-US")
            .with_url(&server.url);
        if let Auth::Entra(entra) = &mut stt.service.auth {
            entra.ttl = Duration::ZERO;
        }
        stt.transcribe(&[0; 320]).await.unwrap();
        stt.transcribe(&[0; 320]).await.unwrap();
        assert_eq!(runs(&count), 2);
    }

    #[tokio::test]
    async fn a_failing_token_command_has_a_code() {
        let server = TestServer::start(vec![success("hi")]).await;
        let failing = argv(&["cmd", "/C", "exit", "2"], &["sh", "-c", "exit 2"]);
        let stt = AzureStt::new(&entra(None, failing), "en-US").with_url(&server.url);
        assert_eq!(
            stt.transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "token_exit_2"))
        );
        let silent = argv(&["cmd", "/C", "echo."], &["sh", "-c", "echo"]);
        let stt = AzureStt::new(&entra(None, silent), "en-US").with_url(&server.url);
        assert_eq!(
            stt.transcribe(&[0; 320]).await,
            Err(SpeechError::new(NAME, "no_token"))
        );
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn text_to_speech_posts_escaped_ssml_and_reads_raw_samples() {
        let server = TestServer::start(vec![Canned::new(
            200,
            "audio/x-wav",
            vec![0x01, 0x00, 0xff, 0xff, 0x05],
        )])
        .await;
        let samples = keyed_tts(&server, None)
            .synthesize("a < b & \"c\" 'd' > e")
            .await
            .unwrap();
        assert_eq!(samples, vec![1, -1]);
        let request = &server.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/tts");
        assert_eq!(request.header(KEY_HEADER), Some("key-abc"));
        assert_eq!(request.header("content-type"), Some("application/ssml+xml"));
        assert_eq!(
            request.header("x-microsoft-outputformat"),
            Some("raw-16khz-16bit-mono-pcm")
        );
        assert_eq!(request.header("user-agent"), Some("push-agent-bridge"));
        assert_eq!(
            String::from_utf8(request.body.clone()).unwrap(),
            "<speak version=\"1.0\" xmlns=\"http://www.w3.org/2001/10/synthesis\" xml:lang=\"en-US\"><voice name=\"en-US-JennyNeural\">a &lt; b &amp; &quot;c&quot; &apos;d&apos; &gt; e</voice></speak>"
        );
    }

    #[tokio::test]
    async fn text_to_speech_uses_the_configured_voice() {
        let server = TestServer::start(vec![Canned::new(200, "audio/x-wav", Vec::new())]).await;
        keyed_tts(&server, Some("de-DE-KatjaNeural"))
            .synthesize("Hallo")
            .await
            .unwrap();
        let body = String::from_utf8(server.requests()[0].body.clone()).unwrap();
        assert!(
            body.contains("<voice name=\"de-DE-KatjaNeural\">Hallo</voice>"),
            "{body}"
        );
        let server = TestServer::start(vec![Canned::new(503, "text/plain", "")]).await;
        assert_eq!(
            keyed_tts(&server, None).synthesize("x").await,
            Err(SpeechError::new(NAME, "http_503"))
        );
    }
}
