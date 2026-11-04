//! Both message decoders on arbitrary bytes as a control payload: they never panic, and a
//! message they accept encodes and decodes back to itself.

#![no_main]

use libfuzzer_sys::fuzz_target;
use push_voice_adapter_proto::{
    AdapterMessage, BridgeMessage, Decoded, Frame, Message, MessageError, decode, encode,
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

fuzz_target!(|json: &[u8]| {
    round_trip::<BridgeMessage>(json);
    round_trip::<AdapterMessage>(json);
});
