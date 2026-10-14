//! push-gateway: terminates client WebSockets (spec §2). It decodes frames, forwards them
//! to message-svc over gRPC, and pushes new entries to connected devices when Redis rings
//! their user's channel. It holds no database credentials and no business rules.

mod config;
mod hub;
mod limits;
mod server;
mod session;
mod tokens;

pub use config::{Config, ConfigError};
pub use hub::{Hub, Membership, Signal};
pub use limits::{
    Action, Counter, FrameBudget, Limit, Limits, Outcome, RedisCounter, Refused, Scope, Subject,
};
pub use server::{init_logging, serve, terminated};
pub use session::{RetryDelay, Session, Step};
pub use tokens::{Claims, LEEWAY, REFETCH_INTERVAL, TokenError, Verifier};
