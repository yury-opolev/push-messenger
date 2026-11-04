//! The scripted adapter against a hand-driven bridge over in-memory pipes.

use std::time::Duration;

use push_voice_adapter_proto::{
    AdapterHello, AdapterMessage, AsyncFrameReader, AsyncFrameWriter, AudioFrame, BridgeHello,
    BridgeMessage, CallEndReason, CallEnded, CallStarted, Decoded, ErrorCode, Frame, Ping, Speak,
    StopSpeaking, Stream, decode, limits,
};
use push_voice_adapter_testkit::detector::END_SILENCE_FRAMES;
use push_voice_adapter_testkit::tones::{FRAME_SAMPLES, SPEAK_AMPLITUDE, amplitude, silence, tone};
use push_voice_adapter_testkit::{
    AdapterError, Event, InProcess, Log, Outcome, Script, Word, duplex,
};
use tokio::io::DuplexStream;
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

const WAIT: Duration = Duration::from_secs(60);

#[derive(Debug)]
enum In {
    Message(AdapterMessage),
    Audio(AudioFrame),
}

struct Bridge {
    reader: AsyncFrameReader<DuplexStream>,
    writer: AsyncFrameWriter<DuplexStream>,
    log: Log,
    task: JoinHandle<Result<Outcome, AdapterError>>,
    seq: u32,
}

impl Bridge {
    fn start(script: Script) -> Self {
        let InProcess {
            reader,
            writer,
            log,
            task,
        } = duplex(script);
        Self {
            reader: AsyncFrameReader::new(reader),
            writer: AsyncFrameWriter::new(writer),
            log,
            task,
            seq: 0,
        }
    }

    async fn send(&mut self, message: BridgeMessage) {
        self.writer.write_message(&message).await.unwrap();
    }

    async fn next(&mut self) -> Option<In> {
        match timeout(WAIT, self.reader.read_frame())
            .await
            .expect("the adapter answers")
            .unwrap()?
        {
            Frame::Control(json) => match decode::<AdapterMessage>(&json).unwrap() {
                Decoded::Known(message) => Some(In::Message(message)),
                Decoded::Unknown(name) => panic!("unknown message {name}"),
            },
            Frame::Audio(audio) => Some(In::Audio(audio)),
        }
    }

    async fn message(&mut self) -> AdapterMessage {
        match self.next().await {
            Some(In::Message(message)) => message,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    async fn hello(&mut self) -> AdapterHello {
        self.send(BridgeMessage::Hello(BridgeHello::v1("0.1.0", "en-US")))
            .await;
        match self.message().await {
            AdapterMessage::Hello(hello) => hello,
            other => panic!("expected a hello, got {other:?}"),
        }
    }

    async fn call(&mut self, call_id: &str) {
        self.send(BridgeMessage::CallStarted(CallStarted {
            call_id: call_id.into(),
            peer: "Owner".into(),
            language: "en-US".into(),
        }))
        .await;
        self.seq = 0;
    }

    async fn audio(&mut self, samples: &[i16]) {
        for chunk in samples.chunks(FRAME_SAMPLES) {
            let frame = Frame::Audio(AudioFrame {
                stream: Stream::FromCaller,
                seq: self.seq,
                timestamp_ms: self.seq * 20,
                samples: chunk.to_vec(),
            });
            self.seq += 1;
            self.writer.write_frame(&frame).await.unwrap();
        }
    }

    async fn speak(&mut self, speak_id: &str, text: &str) {
        self.send(BridgeMessage::Speak(Speak {
            call_id: "c1".into(),
            speak_id: speak_id.into(),
            text: text.into(),
            interruptible: true,
            voice: None,
        }))
        .await;
    }

    /// Ends the bridge's side and waits for the adapter to finish.
    async fn close(self) -> Result<Outcome, AdapterError> {
        drop(self.writer);
        drop(self.reader);
        timeout(WAIT, self.task).await.unwrap().unwrap()
    }
}

fn script() -> Script {
    Script::with_words(vec![
        Word::new(800.0, "run the tests"),
        Word::new(1000.0, "yes"),
    ])
}

fn utterance(hz: f64) -> Vec<i16> {
    let mut audio = silence(10);
    audio.extend(tone(hz, 45));
    audio.extend(silence(50));
    audio
}

#[tokio::test(start_paused = true)]
async fn the_hello_carries_the_scripts_capabilities() {
    let mut bridge = Bridge::start(script());
    let hello = bridge.hello().await;
    assert_eq!(hello.protocol, "push-voice-adapter/1");
    assert_eq!(hello.name, "push-voice-adapter-testkit");
    assert_eq!(hello.version, env!("CARGO_PKG_VERSION"));
    assert!(hello.capabilities.stt && hello.capabilities.tts && hello.capabilities.vad);
    assert!(hello.capabilities.barge_in && !hello.capabilities.partials);
    assert!(
        bridge
            .log
            .find(|e| matches!(e, Event::HelloReceived { language, .. } if language == "en-US"))
            .is_some()
    );
    assert_eq!(bridge.close().await.unwrap(), Outcome::Closed);
}

#[tokio::test(start_paused = true)]
async fn a_tone_becomes_speech_started_and_a_final_transcript() {
    let mut bridge = Bridge::start(script());
    bridge.hello().await;
    bridge.call("c1").await;
    bridge.audio(&utterance(800.0)).await;
    let AdapterMessage::SpeechStarted(started) = bridge.message().await else {
        panic!("speech_started first");
    };
    assert_eq!(started.call_id, "c1");
    let AdapterMessage::Transcript(transcript) = bridge.message().await else {
        panic!("then the transcript");
    };
    assert_eq!(transcript.utterance_id, started.utterance_id);
    assert_eq!(transcript.text, "run the tests");
    assert!(transcript.is_final);

    // Two words in one utterance, in order; the next utterance gets a new id.
    let mut audio = tone(1000.0, 10);
    audio.extend(tone(800.0, 10));
    audio.extend(silence(END_SILENCE_FRAMES));
    bridge.audio(&audio).await;
    let AdapterMessage::SpeechStarted(second) = bridge.message().await else {
        panic!("speech_started");
    };
    assert_ne!(second.utterance_id, started.utterance_id);
    let AdapterMessage::Transcript(transcript) = bridge.message().await else {
        panic!("the transcript");
    };
    assert_eq!(transcript.text, "yes run the tests");

    let log = bridge.log.clone();
    assert_eq!(log.count(|e| matches!(e, Event::Audio { .. })), 105 + 35);
    let sent = log
        .find(|e| {
            matches!(
                e,
                Event::TranscriptSent {
                    is_final: true,
                    chars: 13,
                    ..
                }
            )
        })
        .expect("the transcript is logged by length");
    assert!(sent.at <= Instant::now());
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn partials_and_no_vad_follow_the_capabilities() {
    let mut script = script();
    script.capabilities.vad = false;
    script.capabilities.partials = true;
    let mut bridge = Bridge::start(script);
    bridge.hello().await;
    bridge.call("c1").await;
    bridge.audio(&utterance(1000.0)).await;
    let AdapterMessage::Transcript(partial) = bridge.message().await else {
        panic!("a partial, no speech_started");
    };
    assert!(!partial.is_final);
    assert_eq!(partial.text, "yes");
    let AdapterMessage::Transcript(last) = bridge.message().await else {
        panic!("the final");
    };
    assert!(last.is_final);
    assert_eq!(last.utterance_id, partial.utterance_id);
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_speak_is_a_600_hz_tone_in_real_time_then_speak_done() {
    let mut bridge = Bridge::start(script());
    bridge.hello().await;
    bridge.call("c1").await;
    let started = Instant::now();
    bridge.speak("s1", "hello").await;
    // 5 chars × 40 ms = 200 ms = 10 frames.
    let mut audio = Vec::new();
    for seq in 0..10u32 {
        let Some(In::Audio(frame)) = bridge.next().await else {
            panic!("frame {seq}");
        };
        assert_eq!(frame.stream, Stream::ToCaller);
        assert_eq!((frame.seq, frame.timestamp_ms), (seq, seq * 20));
        assert_eq!(frame.samples.len(), FRAME_SAMPLES);
        audio.extend(frame.samples);
    }
    let AdapterMessage::SpeakDone(done) = bridge.message().await else {
        panic!("speak_done after the audio");
    };
    assert_eq!((done.speak_id.as_str(), done.interrupted), ("s1", false));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(180) && elapsed <= Duration::from_millis(220),
        "{elapsed:?}"
    );
    assert!((amplitude(&audio, 600.0) - SPEAK_AMPLITUDE).abs() < 150.0);

    // Speaks queue in order; seq goes on.
    bridge.speak("s2", "a").await;
    bridge.speak("s3", "b").await;
    let mut order = Vec::new();
    while order.len() < 2 {
        match bridge.next().await {
            Some(In::Audio(frame)) => assert!(frame.seq >= 10),
            Some(In::Message(AdapterMessage::SpeakDone(done))) => order.push(done.speak_id),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(order, ["s2", "s3"]);
    let speak = bridge
        .log
        .find(|e| matches!(e, Event::Speak { speak_id, .. } if speak_id == "s1"))
        .unwrap();
    assert!(matches!(
        speak.event,
        Event::Speak {
            chars: 5,
            interruptible: true,
            ..
        }
    ));
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_speak_lead_sends_its_first_frames_at_once() {
    let script = Script {
        speak_lead_ms: 100,
        ..script()
    };
    let mut bridge = Bridge::start(script);
    bridge.hello().await;
    bridge.call("c1").await;
    let started = Instant::now();
    bridge.speak("s1", "hello").await;
    // 10 frames: the first and 5 more (100 ms) at once, then one every 20 ms.
    let mut arrivals = Vec::new();
    for seq in 0..10u32 {
        let Some(In::Audio(frame)) = bridge.next().await else {
            panic!("frame {seq}");
        };
        assert_eq!(frame.seq, seq);
        arrivals.push(started.elapsed());
    }
    assert!(
        arrivals[..6].iter().all(|at| *at == Duration::ZERO),
        "{arrivals:?}"
    );
    assert_eq!(arrivals[6], Duration::from_millis(20), "{arrivals:?}");
    assert_eq!(arrivals[9], Duration::from_millis(80), "{arrivals:?}");
    let AdapterMessage::SpeakDone(done) = bridge.message().await else {
        panic!("speak_done after the audio");
    };
    assert!(!done.interrupted);
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stop_speaking_cuts_and_drops_with_interrupted_speak_dones() {
    let mut bridge = Bridge::start(script());
    bridge.hello().await;
    bridge.call("c1").await;
    bridge.speak("s1", &"x".repeat(100)).await;
    bridge.speak("s2", "queued").await;
    bridge.speak("s3", "queued too").await;
    let Some(In::Audio(_)) = bridge.next().await else {
        panic!("s1 plays");
    };
    // One speak by id (queued), then every other one.
    bridge
        .send(BridgeMessage::StopSpeaking(StopSpeaking {
            call_id: "c1".into(),
            speak_id: Some("s2".into()),
        }))
        .await;
    bridge
        .send(BridgeMessage::StopSpeaking(StopSpeaking {
            call_id: "c1".into(),
            speak_id: None,
        }))
        .await;
    let mut done = Vec::new();
    while done.len() < 3 {
        match bridge.next().await {
            Some(In::Audio(_)) => {}
            Some(In::Message(AdapterMessage::SpeakDone(d))) => {
                assert!(d.interrupted, "{d:?}");
                done.push(d.speak_id);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(done, ["s2", "s1", "s3"]);
    // Nothing more plays: the next thing is the pong.
    bridge.send(BridgeMessage::Ping(Ping { id: 9 })).await;
    let AdapterMessage::Pong(pong) = bridge.message().await else {
        panic!("a pong, no more audio");
    };
    assert_eq!(pong.id, 9);
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn calls_are_one_at_a_time_and_end_with_their_speaks() {
    let mut bridge = Bridge::start(script());
    bridge.hello().await;
    bridge.call("c1").await;
    bridge.call("c2").await;
    let AdapterMessage::Error(busy) = bridge.message().await else {
        panic!("busy");
    };
    assert_eq!(busy.code, ErrorCode::Busy);
    bridge.speak("s1", &"x".repeat(100)).await;
    let Some(In::Audio(_)) = bridge.next().await else {
        panic!("s1 plays");
    };
    bridge
        .send(BridgeMessage::CallEnded(CallEnded {
            call_id: "c1".into(),
            reason: CallEndReason::Completed,
        }))
        .await;
    loop {
        match bridge.next().await {
            Some(In::Audio(_)) => {}
            Some(In::Message(AdapterMessage::SpeakDone(done))) => {
                assert!(done.interrupted);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    // A speak for the ended call is unknown; a new call starts.
    bridge.speak("s2", "late").await;
    let AdapterMessage::Error(unknown) = bridge.message().await else {
        panic!("unknown_call");
    };
    assert_eq!(unknown.code, ErrorCode::UnknownCall);
    bridge.call("c2").await;
    bridge.send(BridgeMessage::Ping(Ping { id: 1 })).await;
    assert!(matches!(bridge.message().await, AdapterMessage::Pong(_)));
    assert!(
        bridge
            .log
            .find(|e| matches!(e, Event::CallStarted { call_id } if call_id == "c2"))
            .is_some()
    );
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn wrong_size_audio_is_unsupported_format_once() {
    let mut bridge = Bridge::start(script());
    bridge.hello().await;
    bridge.call("c1").await;
    for _ in 0..3 {
        bridge.audio(&[0; 100]).await;
    }
    let AdapterMessage::Error(error) = bridge.message().await else {
        panic!("unsupported_format");
    };
    assert_eq!(error.code, ErrorCode::UnsupportedFormat);
    assert_eq!(error.call_id.as_deref(), Some("c1"));
    bridge.send(BridgeMessage::Ping(Ping { id: 2 })).await;
    assert!(matches!(bridge.message().await, AdapterMessage::Pong(_)));
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn pings_are_answered_and_sent_every_10_s() {
    let mut bridge = Bridge::start(script());
    bridge.hello().await;
    let started = Instant::now();
    let AdapterMessage::Ping(ping) = bridge.message().await else {
        panic!("a ping");
    };
    assert_eq!(started.elapsed(), limits::PING_INTERVAL);
    bridge.send(BridgeMessage::Ping(Ping { id: 77 })).await;
    let AdapterMessage::Pong(pong) = bridge.message().await else {
        panic!("a pong");
    };
    assert_eq!(pong.id, 77);
    let AdapterMessage::Ping(next) = bridge.message().await else {
        panic!("another ping");
    };
    assert_ne!(next.id, ping.id);
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn crash_after_frames_drops_the_stream() {
    let mut script = script();
    script.crash_after_frames = Some(5);
    let mut bridge = Bridge::start(script);
    bridge.hello().await;
    bridge.call("c1").await;
    bridge.audio(&silence(10)).await;
    let result = timeout(WAIT, &mut bridge.task).await.unwrap().unwrap();
    assert!(matches!(result, Err(AdapterError::Crashed)), "{result:?}");
    assert_eq!(
        bridge.next().await.map(|_| ()),
        None,
        "the stream just ends"
    );
    assert!(
        bridge
            .log
            .find(|e| *e == Event::Crashed { frames: 5 })
            .is_some()
    );
}

#[tokio::test(start_paused = true)]
async fn crash_once_marker_crashes_only_the_first_time() {
    let marker = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("testkit-crash-once-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mut script = script();
    script.crash_after_frames = Some(2);
    script.crash_once_marker = Some(marker.clone());

    let mut first = Bridge::start(script.clone());
    first.hello().await;
    first.call("c1").await;
    first.audio(&silence(3)).await;
    let crashed = timeout(WAIT, &mut first.task).await.unwrap().unwrap();
    assert!(matches!(crashed, Err(AdapterError::Crashed)));
    assert!(marker.exists());

    let mut second = Bridge::start(script);
    second.hello().await;
    second.call("c1").await;
    second.audio(&utterance(1000.0)).await;
    assert!(matches!(
        second.message().await,
        AdapterMessage::SpeechStarted(_)
    ));
    assert_eq!(second.close().await.unwrap(), Outcome::Closed);
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test(start_paused = true)]
async fn refuse_format_answers_unsupported_format_and_closes() {
    let mut script = script();
    script.refuse_format = true;
    let mut bridge = Bridge::start(script);
    bridge
        .send(BridgeMessage::Hello(BridgeHello::v1("0.1.0", "en-US")))
        .await;
    let AdapterMessage::Error(error) = bridge.message().await else {
        panic!("an error instead of a hello");
    };
    assert_eq!(error.code, ErrorCode::UnsupportedFormat);
    assert!(bridge.next().await.is_none(), "then it closes");
    assert_eq!(bridge.close().await.unwrap(), Outcome::Refused);
}

#[tokio::test(start_paused = true)]
async fn a_bridge_offering_only_other_versions_is_refused() {
    let mut bridge = Bridge::start(script());
    let mut hello = BridgeHello::v1("0.1.0", "en-US");
    hello.protocol = "push-voice-adapter/9".into();
    hello.protocols = vec!["push-voice-adapter/9".into()];
    bridge.send(BridgeMessage::Hello(hello)).await;
    let AdapterMessage::Error(error) = bridge.message().await else {
        panic!("unsupported_protocol");
    };
    assert_eq!(error.code, ErrorCode::UnsupportedProtocol);
    assert_eq!(bridge.close().await.unwrap(), Outcome::Refused);
}

#[tokio::test(start_paused = true)]
async fn protocol_answers_another_version() {
    let mut script = script();
    script.protocol = Some("push-voice-adapter/2".into());
    let mut bridge = Bridge::start(script);
    assert_eq!(bridge.hello().await.protocol, "push-voice-adapter/2");
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stall_after_hello_never_answers_again() {
    let mut script = script();
    script.stall_after_hello = true;
    let mut bridge = Bridge::start(script);
    bridge.hello().await;
    bridge.send(BridgeMessage::Ping(Ping { id: 1 })).await;
    let silent = timeout(Duration::from_secs(120), bridge.reader.read_frame()).await;
    assert!(silent.is_err(), "no pong, no ping");
    assert!(!bridge.task.is_finished(), "the stream stays open");
    bridge.task.abort();
}

#[tokio::test(start_paused = true)]
async fn flood_messages_sends_pings_at_once() {
    let mut script = script();
    script.flood_messages = 150;
    let mut bridge = Bridge::start(script);
    bridge.hello().await;
    let started = Instant::now();
    for _ in 0..150 {
        assert!(matches!(bridge.message().await, AdapterMessage::Ping(_)));
    }
    assert!(started.elapsed() < Duration::from_millis(10));
    bridge.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn no_hello_never_answers_until_the_bridge_leaves() {
    let mut script = script();
    script.no_hello = true;
    let mut bridge = Bridge::start(script);
    bridge
        .send(BridgeMessage::Hello(BridgeHello::v1("0.1.0", "en-US")))
        .await;
    let silent = timeout(limits::HELLO_TIMEOUT * 2, bridge.reader.read_frame()).await;
    assert!(silent.is_err(), "no hello");
    assert_eq!(bridge.close().await.unwrap(), Outcome::Closed);
}

/// push-bridge's PR 7 voice fixtures give the transcripts its fake speech-to-text gives:
/// each `say` step is one utterance of its word.
#[test]
fn the_bridge_voice_fixtures_hear_their_words() {
    use push_voice_adapter_testkit::detector::{Detector, Heard};
    use push_voice_adapter_testkit::tones::frames;

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../push-bridge/tests/fixtures/voice");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let words: Vec<Word> =
            serde_json::from_value(fixture["words"].clone()).expect("words as in a script");
        let word_at = |hz: f64| {
            words
                .iter()
                .find(|word| word.hz == hz)
                .map(|word| word.word.clone())
                .unwrap()
        };
        let mut audio = Vec::new();
        let mut expected = Vec::new();
        for step in fixture["script"].as_array().unwrap() {
            if let Some(say) = step.get("say") {
                let hz = say["hz"].as_f64().unwrap();
                audio.extend(tone(hz, say["ms"].as_u64().unwrap() as usize / 20));
                expected.push(word_at(hz));
            } else {
                audio.extend(silence(
                    step["silence"]["ms"].as_u64().unwrap() as usize / 20,
                ));
            }
        }
        let mut detector = Detector::new(words.clone());
        let heard: Vec<String> = frames(&audio)
            .iter()
            .flat_map(|frame| detector.push(frame))
            .filter_map(|heard| match heard {
                Heard::Final(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(heard, expected, "{}", path.display());
        checked += 1;
    }
    assert!(checked >= 5, "the fixtures are found");
}
