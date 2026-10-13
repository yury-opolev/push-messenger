//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change.

pub mod api;
mod config;
mod db;
mod devices;
mod error;
pub mod limits;
pub mod links;
pub mod outbox;
pub mod password;
mod profiles;
pub mod rules;
mod server;
mod service;
pub mod tokens;

pub use config::{Config, ConfigError};
pub use db::{MIGRATOR, migrate_database};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, REFRESH_GRACE, Tokens};
pub use error::AuthError;
pub use profiles::{OwnProfile, PublicProfile};
pub use server::{init_logging, serve, terminated};
pub use service::{
    Auth, LINK_SECONDS, REGISTRATION_TOKEN_SECONDS, RegistrationToken, Settings, Signup,
};
