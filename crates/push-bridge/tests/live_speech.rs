//! The configured speech providers against the real services (opt in, never in CI):
//! text-to-speech says a sentence, speech-to-text hears it back. Works for `azure-speech`
//! and the local engines.
//!
//! `PUSH_BRIDGE_LIVE_SPEECH=1` and `PUSH_BRIDGE_LIVE_SPEECH_CONFIG=<file>`: a JSON file with
//! a `calls` section as in `config.json` (or the section alone). With speech-to-text only,
//! `PUSH_BRIDGE_LIVE_SPEECH_WAV=<file>` is what it transcribes. Keys come from the
//! environment as the section says. It prints provider names and timings only.

use std::path::Path;
use std::time::Instant;

use push_bridge::config::CallsConfig;
use push_bridge::voice::{SAMPLE_RATE, provider, wav};
use serde_json::Value;

const SENTENCE: &str = "Please run the unit tests now.";

fn calls_config() -> Option<CallsConfig> {
    if std::env::var("PUSH_BRIDGE_LIVE_SPEECH").as_deref() != Ok("1") {
        eprintln!("skipped: set PUSH_BRIDGE_LIVE_SPEECH=1 to try the configured speech providers");
        return None;
    }
    let Ok(path) = std::env::var("PUSH_BRIDGE_LIVE_SPEECH_CONFIG") else {
        eprintln!("skipped: set PUSH_BRIDGE_LIVE_SPEECH_CONFIG to a file with a calls section");
        return None;
    };
    let text = std::fs::read_to_string(&path).expect("the config file is readable");
    let mut value: Value = serde_json::from_str(&text).expect("the config file is JSON");
    if let Some(section) = value.get_mut("calls") {
        value = section.take();
    }
    let calls: CallsConfig = serde_json::from_value(value).expect("a valid calls section");
    calls.validate().expect("the calls section validates");
    Some(calls)
}

/// The root mean square of `samples`.
fn rms(samples: &[i16]) -> f64 {
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / samples.len().max(1) as f64).sqrt()
}

#[tokio::test]
async fn the_configured_providers_speak_and_listen() {
    let Some(calls) = calls_config() else {
        return;
    };
    let providers = provider::build(&calls);
    println!(
        "stt {}, tts {}",
        providers.stt.as_ref().map_or("none", |stt| stt.name()),
        providers.tts.as_ref().map_or("none", |tts| tts.name()),
    );
    let samples = match &providers.tts {
        Some(tts) => {
            let started = Instant::now();
            let samples = tts
                .synthesize(SENTENCE)
                .await
                .expect("text-to-speech works");
            let seconds = samples.len() as f64 / f64::from(SAMPLE_RATE);
            println!(
                "tts: {seconds:.2} s of audio in {} ms",
                started.elapsed().as_millis()
            );
            assert!(seconds >= 0.5, "the sentence takes {seconds:.2} s");
            assert!(rms(&samples) > 100.0, "the speech is not silent");
            samples
        }
        None => {
            let Ok(path) = std::env::var("PUSH_BRIDGE_LIVE_SPEECH_WAV") else {
                eprintln!(
                    "skipped: no text-to-speech; set PUSH_BRIDGE_LIVE_SPEECH_WAV to a recording"
                );
                return;
            };
            let bytes = std::fs::read(Path::new(&path)).expect("the WAV file is readable");
            let (rate, samples) = wav::decode(&bytes).expect("a 16-bit PCM WAV");
            wav::resample(&samples, rate, SAMPLE_RATE)
        }
    };
    let Some(stt) = &providers.stt else {
        println!("no speech-to-text configured: done");
        return;
    };
    let started = Instant::now();
    let transcript = stt
        .transcribe(&samples)
        .await
        .expect("speech-to-text works");
    println!(
        "stt: {} characters in {} ms",
        transcript.chars().count(),
        started.elapsed().as_millis()
    );
    assert!(
        transcript.to_lowercase().contains("test"),
        "the transcript has \"test\" in it"
    );
}
