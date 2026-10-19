//! push-auth: the auth service of push-messenger (spec §2, §5).
//!
//! It owns the `auth` schema in its own database (`push_auth`): accounts, devices and
//! refresh tokens, plus an outbox of events for the other services, written in the same
//! transaction as each change. Whose profiles a caller may see it asks message-svc, which
//! knows the contacts and conversations (spec §5.5).

pub mod admin;
pub mod api;
mod audit;
mod config;
mod db;
mod devices;
mod directory;
pub mod entra;
mod error;
mod external;
pub mod identity;
pub mod limits;
pub mod links;
pub mod outbox;
pub mod password;
mod profiles;
pub mod rules;
mod server;
mod service;
pub mod tokens;

pub use admin::{AdminApi, UserSelector, admin_router};
/// The attribute an [`Authenticator`] or `Admission` implementation needs, so a binary of
/// one's own uses the same version.
pub use async_trait::async_trait;
pub use config::{Config, ConfigError, MigrateConfig};
pub use db::{
    MIGRATOR, grant_worker_access, grant_worker_access_on, is_role_name, migrate_database,
};
pub use devices::{Caller, DeviceInfo, MAX_ACTIVE_DEVICES, REFRESH_GRACE, Tokens};
pub use directory::{Directory, GrpcDirectory, NoDirectory};
pub use error::AuthError;
pub use external::EntraRegistration;
pub use identity::{
    Admission, AdmissionDecision, AuthenticationError, Authenticator, ENTRA_PROVIDER, Principal,
};
pub use profiles::{OwnProfile, PublicProfile};
pub use server::{init_logging, migrate, serve, terminated};
pub use service::{
    Auth, LINK_SECONDS, REGISTRATION_TOKEN_SECONDS, RegistrationToken, Settings, SignIn, Signup,
};

#[cfg(test)]
mod tests;
