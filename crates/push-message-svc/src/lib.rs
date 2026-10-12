//! push-message-svc: the messaging core of push-messenger (spec §2) as a gRPC service.
//!
//! It owns the `messaging` schema (through `push-store`), answers the gateway's calls
//! and, after every commit, rings the affected users' Redis channels so their gateways
//! sync them.

mod config;
mod convert;
mod grpc;
mod notify;
mod server;

pub use config::{Config, ConfigError};
pub use grpc::{DevSetupApi, MessagingApi};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
