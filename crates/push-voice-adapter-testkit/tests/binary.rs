//! The `push-voice-adapter-testkit` binary: `--dump-env`, stdio and connect modes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use push_bridge_control::Endpoint;
use push_bridge_control::transport::connect_same_user;
use push_voice_adapter_proto::{
    AdapterMessage, AsyncFrameReader, AsyncFrameWriter, AudioFrame, BridgeHello, BridgeMessage,
    CallStarted, Decoded, ErrorCode, Frame, Stream, decode,
};
use push_voice_adapter_testkit::tones::{FRAME_SAMPLES, silence, tone};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;
use tokio::time::timeout;

const EXE: &str = env!("CARGO_BIN_EXE_push-voice-adapter-testkit");
const WAIT: Duration = Duration::from_secs(20);

/// A fresh scratch folder under the target dir.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "tk-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn script_file(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join("script.json");
    std::fs::write(&path, json).unwrap();
    path
}

const SCRIPT: &str = r#"{"words": [{"hz": 1000, "word": "approve"}]}"#;

struct Bridge<R, W> {
    reader: AsyncFrameReader<R>,
    writer: AsyncFrameWriter<W>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Bridge<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self {
            reader: AsyncFrameReader::new(reader),
            writer: AsyncFrameWriter::new(writer),
        }
    }

    async fn send(&mut self, message: BridgeMessage) {
        self.writer.write_message(&message).await.unwrap();
    }

    async fn message(&mut self) -> Option<AdapterMessage> {
        loop {
            let frame = timeout(WAIT, self.reader.read_frame())
                .await
                .expect("the adapter answers")
                .ok()??;
            if let Frame::Control(json) = frame {
                if let Decoded::Known(message) = decode::<AdapterMessage>(&json).unwrap() {
                    return Some(message);
                }
            }
        }
    }

    async fn hello(&mut self) -> AdapterMessage {
        self.send(BridgeMessage::Hello(BridgeHello::v1("0.1.0", "en-US")))
            .await;
        self.message().await.expect("an answer")
    }

    async fn audio(&mut self, samples: &[i16]) {
        for (seq, chunk) in samples.chunks(FRAME_SAMPLES).enumerate() {
            let frame = Frame::Audio(AudioFrame {
                stream: Stream::FromCaller,
                seq: seq as u32,
                timestamp_ms: seq as u32 * 20,
                samples: chunk.to_vec(),
            });
            self.writer.write_frame(&frame).await.unwrap();
        }
    }

    async fn call(&mut self) {
        self.send(BridgeMessage::CallStarted(CallStarted {
            call_id: "c1".into(),
            peer: String::new(),
            language: "en-US".into(),
        }))
        .await;
    }
}

#[tokio::test]
async fn dump_env_prints_argv_and_the_environment() {
    let output = Command::new(EXE)
        .args(["--dump-env", "a b; echo pwned", ""])
        .env("TESTKIT_PROBE", "here")
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines = text.lines();
    let argv: Vec<String> = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(argv[1..], ["--dump-env", "a b; echo pwned", ""]);
    let env: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(env["TESTKIT_PROBE"], "here");
    assert_eq!(lines.next(), None);
}

#[tokio::test]
async fn stdio_mode_hears_a_word_and_logs_events() {
    let dir = scratch("stdio");
    let events = dir.join("events.jsonl");
    let mut child = Command::new(EXE)
        .arg("--script")
        .arg(script_file(&dir, SCRIPT))
        .arg("--events")
        .arg(&events)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut bridge = Bridge::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    assert!(matches!(bridge.hello().await, AdapterMessage::Hello(_)));
    bridge.call().await;
    let mut audio = tone(1000.0, 30);
    audio.extend(silence(20));
    bridge.audio(&audio).await;
    assert!(matches!(
        bridge.message().await,
        Some(AdapterMessage::SpeechStarted(_))
    ));
    let Some(AdapterMessage::Transcript(transcript)) = bridge.message().await else {
        panic!("a transcript");
    };
    assert_eq!(transcript.text, "approve");
    drop(bridge);
    let status = timeout(WAIT, child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(0));
    let log = std::fs::read_to_string(&events).unwrap();
    let names: Vec<String> = log
        .lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(value["t_us"].is_u64(), "{line}");
            value["event"].as_str().unwrap().to_owned()
        })
        .collect();
    for name in [
        "hello_received",
        "hello_sent",
        "call_started",
        "audio",
        "speech_started_sent",
        "transcript_sent",
        "closed",
    ] {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }
    assert!(!log.contains("approve"), "content-free: {log}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_scripted_crash_exits_with_code_3() {
    let dir = scratch("crash");
    let mut child = Command::new(EXE)
        .arg("--script")
        .arg(script_file(&dir, r#"{"crash_after_frames": 3}"#))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut bridge = Bridge::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    bridge.hello().await;
    bridge.call().await;
    bridge.audio(&silence(3)).await;
    let status = timeout(WAIT, child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(3));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn connect_mode_serves_one_bridge_at_a_time() {
    let dir = scratch("c");
    let (flag, value, endpoint) = if cfg!(windows) {
        let name = format!(
            "push-voice-testkit-{}",
            dir.file_name().unwrap().to_string_lossy()
        );
        let endpoint = Endpoint::Pipe(format!(r"\\.\pipe\{name}"));
        ("--pipe", name, endpoint)
    } else {
        let path = dir.join("a").join("v.sock");
        let value = path.to_string_lossy().into_owned();
        ("--socket", value, Endpoint::Socket(path))
    };
    let _child = Command::new(EXE)
        .arg("--script")
        .arg(script_file(&dir, SCRIPT))
        .args([flag, &value])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let connect = || async {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match connect_same_user(&endpoint).await {
                Ok(stream) => return stream,
                Err(error) if tokio::time::Instant::now() < deadline => {
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("cannot connect: {error}"),
            }
        }
    };

    let (read, write) = tokio::io::split(connect().await);
    let mut first = Bridge::new(read, write);
    assert!(matches!(first.hello().await, AdapterMessage::Hello(_)));

    // A second bridge while the first is served: busy.
    let (read, write) = tokio::io::split(connect().await);
    let mut second = Bridge::new(read, write);
    let AdapterMessage::Error(busy) = second.hello().await else {
        panic!("busy");
    };
    assert_eq!(busy.code, ErrorCode::Busy);
    drop(second);

    // The first bridge goes away; the next one is served (a restarted bridge).
    drop(first);
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let (read, write) = tokio::io::split(connect().await);
        let mut next = Bridge::new(read, write);
        match next.hello().await {
            AdapterMessage::Hello(_) => break,
            AdapterMessage::Error(error) if error.code == ErrorCode::Busy => {
                assert!(tokio::time::Instant::now() < deadline, "still busy");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => panic!("{other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}
