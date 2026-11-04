//! The frame decoder on arbitrary bytes, split into reads of a size chosen by the first
//! byte: it never panics, never buffers past one frame plus one read, and what it decodes
//! encodes back to the same frame. Control payloads go on to both message decoders.

#![no_main]

use libfuzzer_sys::fuzz_target;
use push_voice_adapter_proto::frame::HEADER_LEN;
use push_voice_adapter_proto::limits::MAX_PAYLOAD;
use push_voice_adapter_proto::{
    AdapterMessage, BridgeMessage, Decoded, Frame, FrameDecoder, Message, MessageError, decode,
    encode,
};

/// A message a decoder accepts encodes and decodes back to itself (unless re-encoding
/// grew it past the frame limit, as `0` → `0.0` can).
fn round_trip<M: Message + PartialEq + std::fmt::Debug>(json: &[u8]) {
    if let Ok(Decoded::Known(message)) = decode::<M>(json) {
        match encode(&message) {
            Ok(Frame::Control(again)) => {
                assert_eq!(decode::<M>(&again), Ok(Decoded::Known(message)));
            }
            Err(MessageError::TooLarge { .. }) => {}
            other => panic!("a decoded message does not encode: {other:?}"),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&first, bytes)) = data.split_first() else {
        return;
    };
    let chunk = usize::from(first) + 1;
    let mut decoder = FrameDecoder::new();
    for piece in bytes.chunks(chunk) {
        decoder.push(piece);
        assert!(decoder.buffered() <= HEADER_LEN + MAX_PAYLOAD + chunk);
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    let encoded = frame.to_bytes().expect("a decoded frame encodes");
                    let mut again = FrameDecoder::new();
                    again.push(&encoded);
                    assert_eq!(again.next_frame(), Ok(Some(frame.clone())));
                    if let Frame::Control(json) = &frame {
                        round_trip::<BridgeMessage>(json);
                        round_trip::<AdapterMessage>(json);
                    }
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
});
