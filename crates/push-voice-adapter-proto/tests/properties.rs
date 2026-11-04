//! Property tests ("fuzz-like", run in CI): frames survive any split or merge, arbitrary
//! bytes never panic the decoders, and messages round trip. The cargo-fuzz targets in
//! `fuzz/` explore the same decoders without bounds.

use proptest::prelude::*;
use push_voice_adapter_proto::frame::{MAX_SAMPLES, Stream};
use push_voice_adapter_proto::limits::MAX_PAYLOAD;
use push_voice_adapter_proto::{
    AdapterMessage, AudioFrame, BridgeMessage, CallEndReason, CallEnded, Decoded, Frame,
    FrameDecoder, FrameError, Speak, SpeakDone, Transcript, decode, encode,
};

fn stream() -> impl Strategy<Value = Stream> {
    prop_oneof![Just(Stream::FromCaller), Just(Stream::ToCaller)]
}

fn frame() -> impl Strategy<Value = Frame> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 1..300).prop_map(Frame::Control),
        (
            stream(),
            any::<u32>(),
            any::<u32>(),
            prop::collection::vec(any::<i16>(), 1..400)
        )
            .prop_map(|(stream, seq, timestamp_ms, samples)| {
                Frame::Audio(AudioFrame {
                    stream,
                    seq,
                    timestamp_ms,
                    samples,
                })
            }),
    ]
}

/// Feeds `bytes` in chunks of the given sizes (cycled) and collects every frame.
fn decode_in_chunks(bytes: &[u8], sizes: &[usize]) -> Result<Vec<Frame>, FrameError> {
    let mut decoder = FrameDecoder::new();
    let mut frames = Vec::new();
    let mut at = 0;
    let mut i = 0;
    while at < bytes.len() {
        let size = sizes[i % sizes.len()].max(1);
        let end = (at + size).min(bytes.len());
        decoder.push(&bytes[at..end]);
        at = end;
        i += 1;
        while let Some(frame) = decoder.next_frame()? {
            frames.push(frame);
        }
    }
    Ok(frames)
}

fn id() -> impl Strategy<Value = String> {
    "[!-~]{1,64}"
}

fn text() -> impl Strategy<Value = String> {
    any::<String>().prop_filter_map("1 to 4000 characters", |text| {
        let count = text.chars().count();
        (1..=4000).contains(&count).then_some(text)
    })
}

fn adapter_message() -> impl Strategy<Value = AdapterMessage> {
    prop_oneof![
        (
            id(),
            id(),
            text(),
            any::<bool>(),
            prop::option::of(0.0f32..=1.0)
        )
            .prop_map(|(call_id, utterance_id, text, is_final, confidence)| {
                AdapterMessage::Transcript(Transcript {
                    call_id,
                    utterance_id,
                    text,
                    is_final,
                    confidence,
                    language: None,
                })
            }),
        (id(), id(), any::<bool>()).prop_map(|(call_id, speak_id, interrupted)| {
            AdapterMessage::SpeakDone(SpeakDone {
                call_id,
                speak_id,
                interrupted,
            })
        }),
    ]
}

fn bridge_message() -> impl Strategy<Value = BridgeMessage> {
    prop_oneof![
        (id(), id(), text(), any::<bool>()).prop_map(|(call_id, speak_id, text, interruptible)| {
            BridgeMessage::Speak(Speak {
                call_id,
                speak_id,
                text,
                interruptible,
                voice: None,
            })
        }),
        id().prop_map(|call_id| BridgeMessage::CallEnded(CallEnded {
            call_id,
            reason: CallEndReason::Failed,
        })),
    ]
}

proptest! {
    #[test]
    fn frames_survive_any_split_and_merge(
        frames in prop::collection::vec(frame(), 0..12),
        sizes in prop::collection::vec(1usize..2000, 1..8),
    ) {
        let mut bytes = Vec::new();
        for frame in &frames {
            frame.encode_into(&mut bytes).unwrap();
        }
        prop_assert_eq!(decode_in_chunks(&bytes, &sizes).unwrap(), frames);
    }

    #[test]
    fn arbitrary_bytes_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..4096),
        sizes in prop::collection::vec(1usize..64, 1..4),
    ) {
        if let Ok(frames) = decode_in_chunks(&bytes, &sizes) {
            for frame in frames {
                // What decodes re-encodes to the same frame.
                let again = decode_in_chunks(&frame.to_bytes().unwrap(), &[1 << 20]).unwrap();
                prop_assert_eq!(again, vec![frame.clone()]);
                if let Frame::Control(json) = frame {
                    let _ = decode::<BridgeMessage>(&json);
                    let _ = decode::<AdapterMessage>(&json);
                }
            }
        }
        let _ = decode::<BridgeMessage>(&bytes);
        let _ = decode::<AdapterMessage>(&bytes);
    }

    #[test]
    fn any_header_over_the_limit_is_refused_at_once(len in (MAX_PAYLOAD as u32 + 1)..=u32::MAX, kind in any::<u8>()) {
        let mut decoder = FrameDecoder::new();
        let mut header = len.to_be_bytes().to_vec();
        header.push(kind);
        decoder.push(&header);
        prop_assert!(decoder.next_frame().is_err());
        prop_assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn audio_frames_up_to_the_limit_round_trip(samples in (MAX_SAMPLES - 3)..=MAX_SAMPLES, seq in any::<u32>()) {
        let frame = Frame::Audio(AudioFrame { stream: Stream::ToCaller, seq, timestamp_ms: 0, samples: vec![-7; samples] });
        let bytes = frame.to_bytes().unwrap();
        prop_assert!(bytes.len() <= 5 + MAX_PAYLOAD);
        prop_assert_eq!(decode_in_chunks(&bytes, &[4096]).unwrap(), vec![frame]);
    }

    #[test]
    fn json_like_noise_never_panics_the_message_decoders(
        type_name in prop_oneof![Just("hello".to_owned()), Just("transcript".to_owned()), Just("speak".to_owned()), "[a-z_]{0,12}"],
        fields in prop::collection::btree_map("[a-z_]{1,12}", prop_oneof![
            any::<i64>().prop_map(serde_json::Value::from),
            any::<bool>().prop_map(serde_json::Value::from),
            ".{0,20}".prop_map(serde_json::Value::from),
            Just(serde_json::Value::Null),
        ], 0..8),
    ) {
        let mut object: serde_json::Map<String, serde_json::Value> = fields.into_iter().collect();
        object.insert("type".into(), type_name.into());
        let json = serde_json::to_vec(&object).unwrap();
        let _ = decode::<BridgeMessage>(&json);
        let _ = decode::<AdapterMessage>(&json);
    }

    #[test]
    fn adapter_messages_round_trip(message in adapter_message()) {
        let Frame::Control(json) = encode(&message).unwrap() else { unreachable!() };
        prop_assert_eq!(decode::<AdapterMessage>(&json).unwrap(), Decoded::Known(message));
    }

    #[test]
    fn bridge_messages_round_trip(message in bridge_message()) {
        let Frame::Control(json) = encode(&message).unwrap() else { unreachable!() };
        prop_assert_eq!(decode::<BridgeMessage>(&json).unwrap(), Decoded::Known(message));
    }
}
