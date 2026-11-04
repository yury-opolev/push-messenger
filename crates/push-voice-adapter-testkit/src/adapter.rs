//! One bridge connection: the handshake (and its faults), then the call loop.

use std::collections::VecDeque;
use std::time::Duration;

use push_voice_adapter_proto::limits::PING_INTERVAL;
use push_voice_adapter_proto::{
    AdapterHello, AdapterMessage, AsyncFrameReader, AsyncFrameWriter, AudioFormat, AudioFrame,
    BridgeHello, BridgeMessage, Decoded, ErrorCode, ErrorMessage, Frame, Ping, Pong, ReadError,
    SUPPORTED, SpeakDone, SpeechStarted, Stream, Transcript, decode, negotiate,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at};

use crate::detector::{Detector, Heard};
use crate::log::Event;
use crate::tones::{FRAME_MS, SPEAK_AMPLITUDE, tone_frame};
use crate::{AdapterError, Log, Outcome, Script};

const FRAME_PERIOD: Duration = Duration::from_millis(FRAME_MS as u64);

type Writer<W> = AsyncFrameWriter<W>;

pub(crate) async fn serve<R, W>(
    script: &Script,
    log: &Log,
    reader: R,
    writer: W,
) -> Result<Outcome, AdapterError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut reader = AsyncFrameReader::new(reader);
    let mut writer = AsyncFrameWriter::new(writer);
    let hello = match read_hello(log, &mut reader, &mut writer).await? {
        Ok(hello) => hello,
        Err(outcome) => return Ok(outcome),
    };
    log.push(Event::HelloReceived {
        protocols: hello.offered().iter().map(|v| (*v).to_owned()).collect(),
        language: hello.language.clone(),
    });
    if script.no_hello {
        return drain(log, &mut reader).await;
    }
    if script.refuse_format || AudioFormat::from_spec(&hello.audio) != Some(AudioFormat::V1) {
        return refuse(
            log,
            &mut writer,
            ErrorCode::UnsupportedFormat,
            "only 16 kHz mono s16le in 20 ms frames",
        )
        .await;
    }
    let protocol = match &script.protocol {
        Some(protocol) => protocol.clone(),
        None => match negotiate(hello.offered(), SUPPORTED) {
            Some(protocol) => protocol,
            None => {
                let message = format!("this adapter speaks {}", SUPPORTED.join(", "));
                return refuse(log, &mut writer, ErrorCode::UnsupportedProtocol, &message).await;
            }
        },
    };
    let mut answer = AdapterHello::new(&script.name, &script.version, script.capabilities);
    answer.protocol = protocol.clone();
    answer.voices = script.voices.clone();
    writer.write_message(&AdapterMessage::Hello(answer)).await?;
    log.push(Event::HelloSent { protocol });
    if script.stall_after_hello {
        // Holds both halves of the stream open, forever.
        std::future::pending::<()>().await;
    }
    let mut session = Session {
        script,
        log,
        call: None,
        frames_received: 0,
        next_ping: 1,
    };
    for _ in 0..script.flood_messages {
        session.ping(&mut writer).await?;
    }
    session.run(&mut reader, &mut writer).await
}

/// The bridge's hello, or how the connection ended before one.
async fn read_hello<R, W>(
    log: &Log,
    reader: &mut AsyncFrameReader<R>,
    writer: &mut Writer<W>,
) -> Result<Result<BridgeHello, Outcome>, AdapterError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    loop {
        let json = match read(log, reader, writer).await? {
            None => return Ok(Err(Outcome::Closed)),
            Some(Frame::Audio(_)) => continue,
            Some(Frame::Control(json)) => json,
        };
        match decode::<BridgeMessage>(&json) {
            Ok(Decoded::Known(BridgeMessage::Hello(hello))) => return Ok(Ok(hello)),
            Ok(_) => continue,
            Err(_) => {
                let outcome =
                    refuse(log, writer, ErrorCode::InvalidMessage, "malformed hello").await?;
                return Ok(Err(outcome));
            }
        }
    }
}

/// The next frame; a framing error is answered `invalid_frame`.
async fn read<R, W>(
    log: &Log,
    reader: &mut AsyncFrameReader<R>,
    writer: &mut Writer<W>,
) -> Result<Option<Frame>, AdapterError>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let result = reader.read_frame().await;
    on_read(log, writer, result).await
}

/// Handles what a read returned: at the end the writer is shut down, a framing error is
/// answered `invalid_frame`. Kept apart from the (cancel-safe) read so a `select!` branch
/// never cancels a write.
async fn on_read<W>(
    log: &Log,
    writer: &mut Writer<W>,
    result: Result<Option<Frame>, ReadError>,
) -> Result<Option<Frame>, AdapterError>
where
    W: AsyncWrite + Unpin + Send,
{
    match result {
        Ok(Some(frame)) => Ok(Some(frame)),
        Ok(None) => {
            log.push(Event::Closed);
            let _ = writer.shutdown().await;
            Ok(None)
        }
        Err(error) => {
            if matches!(error, ReadError::Frame(_)) {
                let _ = send_error(
                    log,
                    writer,
                    ErrorMessage::new(ErrorCode::InvalidFrame, "framing error"),
                )
                .await;
                let _ = writer.shutdown().await;
            }
            Err(error.into())
        }
    }
}

/// Reads until the bridge closes, answering nothing.
async fn drain<R>(log: &Log, reader: &mut AsyncFrameReader<R>) -> Result<Outcome, AdapterError>
where
    R: AsyncRead + Unpin + Send,
{
    while reader.read_frame().await?.is_some() {}
    log.push(Event::Closed);
    Ok(Outcome::Closed)
}

/// A fatal error in place of a hello, then close.
async fn refuse<W>(
    log: &Log,
    writer: &mut Writer<W>,
    code: ErrorCode,
    message: &str,
) -> Result<Outcome, AdapterError>
where
    W: AsyncWrite + Unpin + Send,
{
    send_error(log, writer, ErrorMessage::new(code, message)).await?;
    writer.shutdown().await?;
    Ok(Outcome::Refused)
}

async fn send_error<W>(
    log: &Log,
    writer: &mut Writer<W>,
    error: ErrorMessage,
) -> Result<(), AdapterError>
where
    W: AsyncWrite + Unpin + Send,
{
    let code = error.code.as_str().to_owned();
    writer.write_message(&AdapterMessage::Error(error)).await?;
    log.push(Event::ErrorSent { code });
    Ok(())
}

struct Session<'a> {
    script: &'a Script,
    log: &'a Log,
    call: Option<Call>,
    /// Stream-0 frames received on this connection.
    frames_received: u64,
    next_ping: u32,
}

struct Call {
    id: String,
    detector: Detector,
    utterances: u32,
    utterance_id: Option<String>,
    /// Unfinished speaks, the playing one first.
    speaks: VecDeque<SpeakJob>,
    /// The next stream-1 frame's seq.
    out_seq: u32,
    format_error_sent: bool,
}

struct SpeakJob {
    speak_id: String,
    /// Kept in memory for [`Log::said`] only.
    text: String,
    frames: usize,
    sent: usize,
}

impl Session<'_> {
    async fn run<R, W>(
        &mut self,
        reader: &mut AsyncFrameReader<R>,
        writer: &mut Writer<W>,
    ) -> Result<Outcome, AdapterError>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let mut pinger = interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
        pinger.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut speaker = interval(FRAME_PERIOD);
        loop {
            let speaking = self
                .call
                .as_ref()
                .is_some_and(|call| !call.speaks.is_empty());
            tokio::select! {
                biased;
                frame = reader.read_frame() => {
                    let Some(frame) = on_read(self.log, writer, frame).await? else {
                        return Ok(Outcome::Closed);
                    };
                    let was_speaking = speaking;
                    self.on_frame(frame, writer).await?;
                    let speaking = self.call.as_ref().is_some_and(|call| !call.speaks.is_empty());
                    if speaking && !was_speaking {
                        // The first frame (and the script's lead) at once, then one every
                        // 20 ms.
                        for _ in 0..=self.script.speak_lead_frames() {
                            self.speak_frame(writer).await?;
                        }
                        speaker.reset();
                    }
                }
                _ = speaker.tick(), if speaking => self.speak_frame(writer).await?,
                _ = pinger.tick() => self.ping(writer).await?,
            }
        }
    }

    async fn ping<W>(&mut self, writer: &mut Writer<W>) -> Result<(), AdapterError>
    where
        W: AsyncWrite + Unpin + Send,
    {
        let id = self.next_ping;
        self.next_ping = self.next_ping.wrapping_add(1);
        writer
            .write_message(&AdapterMessage::Ping(Ping { id }))
            .await?;
        Ok(())
    }

    async fn on_frame<W>(
        &mut self,
        frame: Frame,
        writer: &mut Writer<W>,
    ) -> Result<(), AdapterError>
    where
        W: AsyncWrite + Unpin + Send,
    {
        match frame {
            Frame::Audio(audio) => self.on_audio(audio, writer).await,
            Frame::Control(json) => match decode::<BridgeMessage>(&json) {
                Ok(Decoded::Known(message)) => self.on_message(message, writer).await,
                Ok(Decoded::Unknown(_)) => Ok(()),
                Err(_) => {
                    send_error(
                        self.log,
                        writer,
                        ErrorMessage::new(ErrorCode::InvalidMessage, "malformed control message"),
                    )
                    .await
                }
            },
        }
    }

    async fn on_message<W>(
        &mut self,
        message: BridgeMessage,
        writer: &mut Writer<W>,
    ) -> Result<(), AdapterError>
    where
        W: AsyncWrite + Unpin + Send,
    {
        match message {
            BridgeMessage::Hello(_) | BridgeMessage::Pong(_) => Ok(()),
            BridgeMessage::Ping(ping) => {
                self.log.push(Event::PingReceived { id: ping.id });
                writer
                    .write_message(&AdapterMessage::Pong(Pong { id: ping.id }))
                    .await?;
                Ok(())
            }
            BridgeMessage::Error(error) => {
                self.log.push(Event::ErrorReceived {
                    code: error.code.as_str().to_owned(),
                });
                Ok(())
            }
            BridgeMessage::CallStarted(started) => {
                self.log.push(Event::CallStarted {
                    call_id: started.call_id.clone(),
                });
                if self.call.is_some() {
                    let busy = ErrorMessage::new(ErrorCode::Busy, "a call is already active")
                        .for_call(started.call_id);
                    return send_error(self.log, writer, busy).await;
                }
                self.call = Some(Call {
                    id: started.call_id,
                    detector: Detector::new(self.script.words.clone()),
                    utterances: 0,
                    utterance_id: None,
                    speaks: VecDeque::new(),
                    out_seq: 0,
                    format_error_sent: false,
                });
                Ok(())
            }
            BridgeMessage::Speak(speak) => {
                let chars = speak.text.chars().count();
                self.log.push(Event::Speak {
                    call_id: speak.call_id.clone(),
                    speak_id: speak.speak_id.clone(),
                    interruptible: speak.interruptible,
                    chars,
                });
                if !self.is_active(&speak.call_id) {
                    return self.unknown_call(speak.call_id, writer).await;
                }
                if !self.script.capabilities.tts {
                    let mut failed =
                        ErrorMessage::new(ErrorCode::SynthesisFailed, "this adapter cannot speak")
                            .for_call(speak.call_id.clone());
                    failed.speak_id = Some(speak.speak_id.clone());
                    send_error(self.log, writer, failed).await?;
                    return self
                        .speak_done(writer, speak.call_id, speak.speak_id, true, 0)
                        .await;
                }
                let frames = self.script.speak_frames(chars);
                if let Some(call) = self.call.as_mut() {
                    call.speaks.push_back(SpeakJob {
                        speak_id: speak.speak_id,
                        text: speak.text,
                        frames,
                        sent: 0,
                    });
                }
                Ok(())
            }
            BridgeMessage::StopSpeaking(stop) => {
                self.log.push(Event::StopSpeaking {
                    call_id: stop.call_id.clone(),
                    speak_id: stop.speak_id.clone(),
                });
                if !self.is_active(&stop.call_id) {
                    return self.unknown_call(stop.call_id, writer).await;
                }
                let Some(call) = self.call.as_mut() else {
                    return Ok(());
                };
                let stopped: Vec<SpeakJob> = match &stop.speak_id {
                    Some(id) => match call.speaks.iter().position(|job| job.speak_id == *id) {
                        Some(index) => call.speaks.remove(index).into_iter().collect(),
                        None => Vec::new(),
                    },
                    None => call.speaks.drain(..).collect(),
                };
                for job in stopped {
                    self.speak_done(writer, stop.call_id.clone(), job.speak_id, true, job.sent)
                        .await?;
                }
                Ok(())
            }
            BridgeMessage::CallEnded(ended) => {
                self.log.push(Event::CallEnded {
                    call_id: ended.call_id.clone(),
                    reason: format!("{:?}", ended.reason).to_lowercase(),
                });
                if !self.is_active(&ended.call_id) {
                    return self.unknown_call(ended.call_id, writer).await;
                }
                if let Some(call) = self.call.take() {
                    for job in call.speaks {
                        self.speak_done(writer, call.id.clone(), job.speak_id, true, job.sent)
                            .await?;
                    }
                }
                Ok(())
            }
        }
    }

    fn is_active(&self, call_id: &str) -> bool {
        self.call.as_ref().is_some_and(|call| call.id == call_id)
    }

    async fn unknown_call<W>(
        &mut self,
        call_id: String,
        writer: &mut Writer<W>,
    ) -> Result<(), AdapterError>
    where
        W: AsyncWrite + Unpin + Send,
    {
        let error = ErrorMessage::new(ErrorCode::UnknownCall, "no such call").for_call(call_id);
        send_error(self.log, writer, error).await
    }

    async fn on_audio<W>(
        &mut self,
        audio: AudioFrame,
        writer: &mut Writer<W>,
    ) -> Result<(), AdapterError>
    where
        W: AsyncWrite + Unpin + Send,
    {
        if audio.stream != Stream::FromCaller {
            return Ok(());
        }
        let Some(call) = self.call.as_mut() else {
            return Ok(());
        };
        self.frames_received += 1;
        if let Some(limit) = self.script.crash_after_frames {
            if self.frames_received >= limit && crash_now(self.script) {
                self.log.push(Event::Crashed {
                    frames: self.frames_received,
                });
                return Err(AdapterError::Crashed);
            }
        }
        self.log.push(Event::Audio {
            call_id: call.id.clone(),
            seq: audio.seq,
            samples: audio.samples.len(),
        });
        if AudioFormat::V1.check(&audio).is_err() {
            if !call.format_error_sent {
                call.format_error_sent = true;
                let error =
                    ErrorMessage::new(ErrorCode::UnsupportedFormat, "frames of 320 samples only")
                        .for_call(call.id.clone());
                return send_error(self.log, writer, error).await;
            }
            return Ok(());
        }
        let capabilities = self.script.capabilities;
        for heard in call.detector.push(&audio.samples) {
            match heard {
                Heard::SpeechStarted => {
                    call.utterances += 1;
                    let utterance_id = format!("u{}", call.utterances);
                    call.utterance_id = Some(utterance_id.clone());
                    if capabilities.vad {
                        writer
                            .write_message(&AdapterMessage::SpeechStarted(SpeechStarted {
                                call_id: call.id.clone(),
                                utterance_id: utterance_id.clone(),
                            }))
                            .await?;
                        self.log.push(Event::SpeechStartedSent {
                            call_id: call.id.clone(),
                            utterance_id,
                        });
                    }
                }
                Heard::Partial(text) => {
                    if capabilities.stt && capabilities.partials {
                        let utterance_id = call.utterance_id.clone().unwrap_or_default();
                        send_transcript(self.log, writer, &call.id, utterance_id, text, false)
                            .await?;
                    }
                }
                Heard::Final(text) => {
                    let utterance_id = call.utterance_id.take().unwrap_or_default();
                    if capabilities.stt {
                        send_transcript(self.log, writer, &call.id, utterance_id, text, true)
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Sends the playing speak's next frame, and its `speak_done` after its last.
    async fn speak_frame<W>(&mut self, writer: &mut Writer<W>) -> Result<(), AdapterError>
    where
        W: AsyncWrite + Unpin + Send,
    {
        let hz = self.script.speak_hz;
        let Some(call) = self.call.as_mut() else {
            return Ok(());
        };
        let Some(job) = call.speaks.front_mut() else {
            return Ok(());
        };
        if job.sent == 0 {
            self.log.said(&job.text);
            self.log.push(Event::SpeakAudioStarted {
                call_id: call.id.clone(),
                speak_id: job.speak_id.clone(),
            });
        }
        let frame = Frame::Audio(AudioFrame {
            stream: Stream::ToCaller,
            seq: call.out_seq,
            timestamp_ms: (job.sent as u32).wrapping_mul(FRAME_MS),
            samples: tone_frame(hz, SPEAK_AMPLITUDE, job.sent).to_vec(),
        });
        writer.write_frame(&frame).await?;
        call.out_seq = call.out_seq.wrapping_add(1);
        job.sent += 1;
        if job.sent >= job.frames {
            let (call_id, sent) = (call.id.clone(), job.sent);
            let job = call.speaks.pop_front().expect("the playing speak");
            self.speak_done(writer, call_id, job.speak_id, false, sent)
                .await?;
        }
        Ok(())
    }

    async fn speak_done<W>(
        &mut self,
        writer: &mut Writer<W>,
        call_id: String,
        speak_id: String,
        interrupted: bool,
        frames: usize,
    ) -> Result<(), AdapterError>
    where
        W: AsyncWrite + Unpin + Send,
    {
        writer
            .write_message(&AdapterMessage::SpeakDone(SpeakDone {
                call_id: call_id.clone(),
                speak_id: speak_id.clone(),
                interrupted,
            }))
            .await?;
        self.log.push(Event::SpeakDoneSent {
            call_id,
            speak_id,
            interrupted,
            frames,
        });
        Ok(())
    }
}

async fn send_transcript<W>(
    log: &Log,
    writer: &mut Writer<W>,
    call_id: &str,
    utterance_id: String,
    text: String,
    is_final: bool,
) -> Result<(), AdapterError>
where
    W: AsyncWrite + Unpin + Send,
{
    let chars = text.chars().count();
    writer
        .write_message(&AdapterMessage::Transcript(Transcript {
            call_id: call_id.to_owned(),
            utterance_id: utterance_id.clone(),
            text,
            is_final,
            confidence: None,
            language: None,
        }))
        .await?;
    log.push(Event::TranscriptSent {
        call_id: call_id.to_owned(),
        utterance_id,
        is_final,
        chars,
    });
    Ok(())
}

/// Whether `crash_after_frames` fires: always without a marker; with one, only when it
/// does not exist yet (it is created now).
fn crash_now(script: &Script) -> bool {
    match &script.crash_once_marker {
        None => true,
        Some(marker) => {
            if marker.exists() {
                return false;
            }
            let _ = std::fs::write(marker, b"crashed\n");
            true
        }
    }
}
