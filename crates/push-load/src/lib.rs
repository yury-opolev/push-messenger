//! push-load: explicitly targeted, bounded verification of a push-messenger deployment
//! through its public API only.
//!
//! `prepare` signs in operator-supplied, verified test accounts, registers their devices
//! and makes sample pairs contacts; `run` connects the prepared devices at a fixed rate
//! for a finite duration and reports connection, heartbeat, message, receipt and
//! reconnect measurements; `calls` places concurrent voice calls (signalling only)
//! between the sample pairs and reports call setup latency. It has no default target,
//! discovers nothing, creates no accounts, never needs auth-svc's signing key or a
//! mailbox, and never prints credentials. Only test a deployment you own or are
//! authorized to test.

pub mod accounts;
pub mod calls;
pub mod cli;
pub mod config;
pub mod http;
pub mod metrics;
pub mod prepare;
pub mod report;
pub mod run;
pub mod socket;
pub mod state;
