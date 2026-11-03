//! The call engine, in message-svc (spec D1): input checks, policy, the state machine of
//! push-domain over compare-and-set Redis state, publishing, the sweeper, audit and
//! metrics.

mod redis;

pub use self::redis::{Applied, DUE_KEY, RedisCalls, Write, call_key, devices_key, user_key};
