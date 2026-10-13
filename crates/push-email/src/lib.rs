//! push-email: email for push-messenger (spec §9).
//!
//! Services ask for email by writing `email.send {user_id, to, template, params}` to their
//! outbox; the worker renders the template here ([`Templates`]) and hands the result to an
//! [`EmailSender`]: [`ResendSender`] in production, [`LogSender`] locally, which only logs
//! that it sent something.
//! Nothing here logs an address, a subject or a body: emails carry sign-in links.

mod resend;
mod sender;
mod templates;

pub use resend::{RESEND_API_URL, ResendSender};
pub use sender::{EmailSender, LogSender, OutgoingEmail, SendError, recipient_domain};
pub use templates::{Rendered, TemplateError, Templates};
