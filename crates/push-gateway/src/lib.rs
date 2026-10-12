//! push-gateway: terminates client WebSockets (spec §2). It decodes frames, forwards them
//! to message-svc over gRPC, and pushes new entries to connected devices when Redis rings
//! their user's channel. It holds no database credentials and no business rules.

mod config;
mod hub;
mod session;

pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership};
pub use session::{Session, Step};
