//! Events for other services, written to `auth.outbox` in the same transaction as the
//! change they announce (spec §2). The worker (Plan 6) reads and deletes them: it sends
//! the emails and relays the user and device events to message-svc.
//!
//! Payloads (JSON):
//!
//! | kind | payload |
//! |---|---|
//! | `email.send` | `{user_id, to, template, params}` |
//! | `user.verified` | `{user_id}` |
//! | `user.password_reset` | `{user_id}`: every device of the user is to be kicked |
//! | `device.registered` | `{user_id, device_id}` |
//! | `device.revoked` | `{user_id, device_id}` |

use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

/// An email to send, by template. The worker renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Email {
    /// "Verify your address": `params.link` verifies it.
    VerifyEmail { link: String, handle: String },
    /// Someone signed up with an address that has a verified account already.
    AccountExists { handle: String },
    /// "Reset your password": `params.link` opens the reset page.
    ResetPassword { link: String },
}

/// How many minutes an email link stays valid; the templates say so.
pub const LINK_MINUTES: u64 = 30;

impl Email {
    fn template(&self) -> &'static str {
        match self {
            Self::VerifyEmail { .. } => "verify_email",
            Self::AccountExists { .. } => "account_exists",
            Self::ResetPassword { .. } => "reset_password",
        }
    }

    fn params(&self) -> Value {
        match self {
            Self::VerifyEmail { link, handle } => {
                json!({"link": link, "handle": handle, "expires_in_minutes": LINK_MINUTES})
            }
            Self::AccountExists { handle } => json!({"handle": handle}),
            Self::ResetPassword { link } => {
                json!({"link": link, "expires_in_minutes": LINK_MINUTES})
            }
        }
    }
}

/// One outbox event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    EmailSend {
        user_id: Uuid,
        to: String,
        email: Email,
    },
    UserVerified {
        user_id: Uuid,
    },
    UserPasswordReset {
        user_id: Uuid,
    },
    DeviceRegistered {
        user_id: Uuid,
        device_id: Uuid,
    },
    DeviceRevoked {
        user_id: Uuid,
        device_id: Uuid,
    },
}

impl Event {
    /// The `kind` column.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EmailSend { .. } => "email.send",
            Self::UserVerified { .. } => "user.verified",
            Self::UserPasswordReset { .. } => "user.password_reset",
            Self::DeviceRegistered { .. } => "device.registered",
            Self::DeviceRevoked { .. } => "device.revoked",
        }
    }

    /// The `payload` column.
    pub fn payload(&self) -> Value {
        match self {
            Self::EmailSend { user_id, to, email } => json!({
                "user_id": user_id,
                "to": to,
                "template": email.template(),
                "params": email.params(),
            }),
            Self::UserVerified { user_id } | Self::UserPasswordReset { user_id } => {
                json!({"user_id": user_id})
            }
            Self::DeviceRegistered { user_id, device_id }
            | Self::DeviceRevoked { user_id, device_id } => {
                json!({"user_id": user_id, "device_id": device_id})
            }
        }
    }
}

/// Writes `event` in the caller's transaction.
pub(crate) async fn write(connection: &mut PgConnection, event: &Event) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO auth.outbox (event_id, kind, payload) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind(event.kind())
        .bind(event.payload())
        .execute(connection)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_events_carry_the_template_and_its_params() {
        let user_id = Uuid::now_v7();
        let event = Event::EmailSend {
            user_id,
            to: "alice@example.com".to_owned(),
            email: Email::VerifyEmail {
                link: "https://api.example.com/v1/auth/verify?token=t".to_owned(),
                handle: "alice".to_owned(),
            },
        };
        assert_eq!(event.kind(), "email.send");
        assert_eq!(
            event.payload(),
            json!({
                "user_id": user_id,
                "to": "alice@example.com",
                "template": "verify_email",
                "params": {
                    "link": "https://api.example.com/v1/auth/verify?token=t",
                    "handle": "alice",
                    "expires_in_minutes": 30,
                },
            })
        );
    }

    #[test]
    fn user_and_device_events_carry_their_ids() {
        let (user_id, device_id) = (Uuid::now_v7(), Uuid::now_v7());
        let cases = [
            (Event::UserVerified { user_id }, "user.verified"),
            (Event::UserPasswordReset { user_id }, "user.password_reset"),
            (
                Event::DeviceRegistered { user_id, device_id },
                "device.registered",
            ),
            (
                Event::DeviceRevoked { user_id, device_id },
                "device.revoked",
            ),
        ];
        for (event, kind) in cases {
            assert_eq!(event.kind(), kind);
            assert_eq!(event.payload()["user_id"], json!(user_id));
        }
        assert_eq!(
            Event::DeviceRevoked { user_id, device_id }.payload()["device_id"],
            json!(device_id)
        );
    }
}
