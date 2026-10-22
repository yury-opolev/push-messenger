//! auth-svc's user and device events, applied in message-svc through its `Accounts` API.

use std::time::Duration;

use push_proto::id_bytes;
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::{AccountAgent, AccountDevice, AccountUser};
use serde_json::Value;
use tonic::codegen::InterceptedService;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};
use uuid::Uuid;

use crate::queue::Outcome;

/// How long connecting to message-svc may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// One of auth-svc's user and device events (spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountEvent {
    AgentEnrolled {
        user: Uuid,
        owner: Uuid,
    },
    UserVerified {
        user: Uuid,
    },
    DeviceRegistered {
        user: Uuid,
        device: Uuid,
    },
    DeviceRevoked {
        user: Uuid,
        device: Uuid,
    },
    /// `reset_at`: auth-svc's time of the reset, in seconds since the Unix epoch; `None` in
    /// an event written before auth-svc sent it.
    PasswordReset {
        user: Uuid,
        reset_at: Option<i64>,
    },
}

impl AccountEvent {
    /// The event in an outbox row: `None` if `kind` is not an account event, an error if
    /// the payload does not fit the kind.
    pub fn parse(kind: &str, payload: &Value) -> Option<Result<Self, String>> {
        let id = |name: &str| {
            payload[name]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or_else(|| format!("{kind}: the payload has no valid {name}"))
        };
        let device = || -> Result<(Uuid, Uuid), String> { Ok((id("user_id")?, id("device_id")?)) };
        Some(match kind {
            "agent.enrolled" => id("user_id").and_then(|user| {
                let owner = id("owner_user_id")?;
                if user == owner {
                    return Err("agent.enrolled: self target".to_owned());
                }
                Ok(Self::AgentEnrolled { user, owner })
            }),
            "user.verified" => id("user_id").map(|user| Self::UserVerified { user }),
            "user.password_reset" => {
                let reset_at = match &payload["reset_at"] {
                    Value::Null => Ok(None),
                    value => value
                        .as_i64()
                        .filter(|seconds| *seconds > 0)
                        .map(Some)
                        .ok_or_else(|| format!("{kind}: the payload has no valid reset_at")),
                };
                id("user_id").and_then(|user| {
                    Ok(Self::PasswordReset {
                        user,
                        reset_at: reset_at?,
                    })
                })
            }
            "device.registered" => {
                device().map(|(user, device)| Self::DeviceRegistered { user, device })
            }
            "device.revoked" => device().map(|(user, device)| Self::DeviceRevoked { user, device }),
            _ => return None,
        })
    }
}

/// message-svc's `Accounts` API. Calls are idempotent and order-independent there, so the
/// queue's at-least-once, any-order delivery is safe.
#[derive(Debug, Clone)]
pub struct Accounts {
    client: AccountsClient<InterceptedService<Channel, Bearer>>,
}

impl Accounts {
    /// A client for message-svc's Accounts API at `url` (`http://host:50052`) that presents `token`, the
    /// shared internal token message-svc requires for this API. It connects on first use
    /// and reconnects by itself.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let bearer = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the Accounts token cannot be sent as metadata".to_owned())?;
        let mut bearer: MetadataValue<Ascii> = bearer;
        // Never indexed by HTTP/2 or shown in header dumps.
        bearer.set_sensitive(true);
        let channel = Endpoint::from_shared(url.to_owned())
            .map_err(|_| "not a valid message-svc URL".to_owned())?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .connect_lazy();
        Ok(Self {
            client: AccountsClient::with_interceptor(channel, Bearer(bearer)),
        })
    }

    /// Applies `event`. A refusal that can never change (a malformed id, a device id of
    /// another user) is poison; anything else is retried.
    pub async fn apply(&self, event: AccountEvent) -> Outcome {
        let mut client = self.client.clone();
        let user_message = |user: Uuid, reset_at: Option<i64>| AccountUser {
            user_id: id_bytes(user),
            reset_at: reset_at.unwrap_or(0),
        };
        let device_message = |user: Uuid, device: Uuid| AccountDevice {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
        };
        let result = match event {
            AccountEvent::AgentEnrolled { user, owner } => client
                .agent_enrolled(AccountAgent {
                    user_id: id_bytes(user),
                    owner_user_id: id_bytes(owner),
                })
                .await
                .map(|_| ()),
            AccountEvent::UserVerified { user } => client
                .user_verified(user_message(user, None))
                .await
                .map(|_| ()),
            AccountEvent::PasswordReset { user, reset_at } => client
                .password_reset(user_message(user, reset_at))
                .await
                .map(|_| ()),
            AccountEvent::DeviceRegistered { user, device } => client
                .device_registered(device_message(user, device))
                .await
                .map(|_| ()),
            AccountEvent::DeviceRevoked { user, device } => client
                .device_revoked(device_message(user, device))
                .await
                .map(|_| ()),
        };
        match result {
            Ok(()) => Outcome::Done,
            Err(status) => outcome(&status),
        }
    }
}

/// Adds `authorization: Bearer <token>` to every call.
#[derive(Clone)]
struct Bearer(MetadataValue<Ascii>);

impl std::fmt::Debug for Bearer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Bearer(<redacted>)")
    }
}

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        Ok(request)
    }
}

/// Refusals that no retry can change are poison; the rest (unavailable, aborted,
/// deadline exceeded, transport errors, and unauthenticated: a token to fix in the
/// configuration) are retried for as long as it takes, since message-svc must learn every
/// account event eventually.
fn outcome(status: &Status) -> Outcome {
    let reason = format!("message-svc answered {:?}", status.code());
    match status.code() {
        Code::InvalidArgument
        | Code::PermissionDenied
        | Code::NotFound
        | Code::AlreadyExists
        | Code::FailedPrecondition
        | Code::OutOfRange => Outcome::Poison(reason),
        _ => Outcome::Failed {
            reason,
            max_attempts: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn account_payloads_parse_and_other_kinds_are_not_ours() {
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        let both = json!({"user_id": user, "device_id": device});
        assert!(matches!(
            AccountEvent::parse(
                "agent.enrolled",
                &json!({"user_id": user, "owner_user_id": device})
            ),
            Some(Ok(_))
        ));
        assert_eq!(
            AccountEvent::parse("device.revoked", &both),
            Some(Ok(AccountEvent::DeviceRevoked { user, device }))
        );
        assert_eq!(
            AccountEvent::parse(
                "user.password_reset",
                &json!({"user_id": user, "reset_at": 1_760_000_000})
            ),
            Some(Ok(AccountEvent::PasswordReset {
                user,
                reset_at: Some(1_760_000_000)
            }))
        );
        // An event written before auth-svc sent the time.
        assert_eq!(
            AccountEvent::parse("user.password_reset", &json!({"user_id": user})),
            Some(Ok(AccountEvent::PasswordReset {
                user,
                reset_at: None
            }))
        );
        for reset_at in [json!("soon"), json!(-5), json!(0)] {
            assert!(matches!(
                AccountEvent::parse(
                    "user.password_reset",
                    &json!({"user_id": user, "reset_at": reset_at})
                ),
                Some(Err(_))
            ));
        }
        assert_eq!(AccountEvent::parse("email.send", &both), None);
        assert!(matches!(
            AccountEvent::parse("device.registered", &json!({"user_id": user})),
            Some(Err(_))
        ));
        assert!(matches!(
            AccountEvent::parse("user.verified", &json!({"user_id": "nope"})),
            Some(Err(_))
        ));
    }

    #[test]
    fn permanent_refusals_are_poison_and_outages_are_retried() {
        assert!(matches!(
            outcome(&Status::permission_denied("not yours")),
            Outcome::Poison(_)
        ));
        assert!(matches!(
            outcome(&Status::invalid_argument("bad id")),
            Outcome::Poison(_)
        ));
        for status in [
            Status::unavailable("down"),
            Status::aborted("conflict"),
            Status::deadline_exceeded("slow"),
            Status::unknown("transport"),
            Status::unauthenticated("wrong token"),
        ] {
            assert!(
                matches!(
                    outcome(&status),
                    Outcome::Failed {
                        max_attempts: None,
                        ..
                    }
                ),
                "{status:?}"
            );
        }
    }
}
