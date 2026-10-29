//! The agent bridge: reaches a local AI coding agent from push-messenger as a private
//! agent contact. A backend-independent core drives a small [`backend::Backend`] trait;
//! Coda is the first backend.

pub mod backend;
pub mod backoff;
pub mod cards;
pub mod coda;
pub mod commands;
pub mod config;
pub mod folders;
pub mod mask;
pub mod render;
pub mod state;

#[cfg(test)]
pub(crate) mod test_dir;
