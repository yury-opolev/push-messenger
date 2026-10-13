//! Delivery webhooks (spec §9): Resend signs them the Svix way; a verified webhook turns
//! into a [`DeliveryEvent`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// How far a webhook's timestamp may be from our clock, either way. Older deliveries are
/// refused as replays; Svix uses the same window.
pub const WEBHOOK_TOLERANCE: Duration = Duration::from_secs(5 * 60);

/// What happened to a sent email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    Delivered,
    /// `permanent` is a hard bounce: the address does not take email.
    Bounced {
        permanent: bool,
    },
    /// The recipient marked it as spam.
    Complained,
}

/// One delivery event from the provider.
#[derive(Clone, PartialEq, Eq)]
pub struct DeliveryEvent {
    pub kind: DeliveryKind,
    /// The provider's id of the email, as its sender returned it.
    pub provider_message_id: String,
    pub recipients: Vec<String>,
    /// When the provider says it happened, if it says.
    pub occurred_at: Option<DateTime<Utc>>,
}

impl DeliveryEvent {
    /// Whether the recipients must not get email any more: a hard bounce or a complaint.
    pub fn suppresses(&self) -> bool {
        matches!(
            self.kind,
            DeliveryKind::Bounced { permanent: true } | DeliveryKind::Complained
        )
    }
}

impl std::fmt::Debug for DeliveryEvent {
    /// Without the addresses.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeliveryEvent")
            .field("kind", &self.kind)
            .field("provider_message_id", &self.provider_message_id)
            .field("recipients", &self.recipients.len())
            .field("occurred_at", &self.occurred_at)
            .finish()
    }
}

/// Why a webhook was refused. The messages are fixed: they never quote the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("the webhook lacks its signature headers")]
    MissingHeaders,
    #[error("the webhook's timestamp is not within the tolerance of our clock")]
    Stale,
    #[error("the webhook's signature does not match")]
    BadSignature,
    #[error("the webhook's body is not a delivery event")]
    Malformed,
}

/// Turns provider webhooks into delivery events (spec §9).
pub trait WebhookVerifier: Send + Sync {
    /// Checks that `body` was signed by the provider within [`WEBHOOK_TOLERANCE`] of
    /// `now`, reading the signature headers through `header`. Returns the webhook's id,
    /// which stays the same when the provider delivers it again.
    fn verify(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        body: &[u8],
        now: SystemTime,
    ) -> Result<String, WebhookError>;

    /// The delivery event in a verified `body`; `None` for events nothing acts on.
    fn parse(&self, body: &[u8]) -> Result<Option<DeliveryEvent>, WebhookError>;
}

/// Resend's webhooks: Svix signatures (`svix-id`, `svix-timestamp`, `svix-signature`) and
/// `email.*` events.
#[derive(Clone)]
pub struct ResendWebhooks {
    key: Vec<u8>,
}

impl std::fmt::Debug for ResendWebhooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResendWebhooks")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl ResendWebhooks {
    /// The webhook's signing secret, as Resend shows it: `whsec_` and base64.
    pub fn new(secret: &str) -> Result<Self, String> {
        let encoded = secret
            .trim()
            .strip_prefix("whsec_")
            .unwrap_or(secret.trim());
        let key = STANDARD
            .decode(encoded)
            .map_err(|_| "the webhook secret must be whsec_ and base64".to_owned())?;
        if key.len() < 16 {
            return Err("the webhook secret is too short".to_owned());
        }
        Ok(Self { key })
    }

    /// The `svix-signature` header for `body`, sent as webhook `id` at `timestamp` (Unix
    /// seconds): what Resend would send. For tests and tools that stand in for Resend.
    pub fn sign(&self, id: &str, timestamp: u64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC takes any key");
        mac.update(format!("{id}.{timestamp}.").as_bytes());
        mac.update(body);
        format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
    }
}

impl WebhookVerifier for ResendWebhooks {
    fn verify(
        &self,
        header: &dyn Fn(&str) -> Option<String>,
        body: &[u8],
        now: SystemTime,
    ) -> Result<String, WebhookError> {
        let (Some(id), Some(timestamp), Some(signatures)) = (
            header("svix-id"),
            header("svix-timestamp"),
            header("svix-signature"),
        ) else {
            return Err(WebhookError::MissingHeaders);
        };
        let sent: u64 = timestamp.trim().parse().map_err(|_| WebhookError::Stale)?;
        let now = now
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WebhookError::Stale)?
            .as_secs();
        if now.abs_diff(sent) > WEBHOOK_TOLERANCE.as_secs() {
            return Err(WebhookError::Stale);
        }
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).map_err(|_| WebhookError::BadSignature)?;
        mac.update(id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.trim().as_bytes());
        mac.update(b".");
        mac.update(body);
        // Several signatures while the secret is rotated: any one of them will do.
        let genuine = signatures.split_whitespace().any(|entry| {
            entry
                .strip_prefix("v1,")
                .and_then(|signature| STANDARD.decode(signature).ok())
                .is_some_and(|signature| mac.clone().verify_slice(&signature).is_ok())
        });
        if genuine {
            Ok(id)
        } else {
            Err(WebhookError::BadSignature)
        }
    }

    fn parse(&self, body: &[u8]) -> Result<Option<DeliveryEvent>, WebhookError> {
        let event: Value = serde_json::from_slice(body).map_err(|_| WebhookError::Malformed)?;
        let kind = match event["type"].as_str() {
            Some("email.delivered") => DeliveryKind::Delivered,
            Some("email.bounced") => DeliveryKind::Bounced {
                permanent: event["data"]["bounce"]["type"] == "Permanent",
            },
            Some("email.complained") => DeliveryKind::Complained,
            Some(_) => return Ok(None),
            None => return Err(WebhookError::Malformed),
        };
        let provider_message_id = event["data"]["email_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or(WebhookError::Malformed)?
            .to_owned();
        let recipients = event["data"]["to"]
            .as_array()
            .map(|to| {
                to.iter()
                    .filter_map(|address| address.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let occurred_at = event["created_at"]
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc));
        Ok(Some(DeliveryEvent {
            kind,
            provider_message_id,
            recipients,
            occurred_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    // Svix's published example: this secret signed this body at this time.
    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
    const ID: &str = "msg_p5jXN8AQM9LWM0D4loKWxJek";
    const TIMESTAMP: u64 = 1_614_265_330;
    const SIGNATURE: &str = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";
    const BODY: &[u8] = br#"{"test": 2432232314}"#;

    fn headers(id: &str, timestamp: u64, signature: &str) -> HashMap<String, String> {
        HashMap::from([
            ("svix-id".to_owned(), id.to_owned()),
            ("svix-timestamp".to_owned(), timestamp.to_string()),
            ("svix-signature".to_owned(), signature.to_owned()),
        ])
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn verify(
        headers: &HashMap<String, String>,
        body: &[u8],
        now: SystemTime,
    ) -> Result<String, WebhookError> {
        let webhooks = ResendWebhooks::new(SECRET).unwrap();
        webhooks.verify(&|name| headers.get(name).cloned(), body, now)
    }

    #[test]
    fn svixs_example_verifies_and_signs_the_same() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert_eq!(
            verify(&genuine, BODY, at(TIMESTAMP + 10)),
            Ok(ID.to_owned())
        );
        // One of several signatures is enough, while the secret is rotated.
        let rotated = headers(ID, TIMESTAMP, &format!("v1,AAAA {SIGNATURE}"));
        assert_eq!(verify(&rotated, BODY, at(TIMESTAMP)), Ok(ID.to_owned()));
        let webhooks = ResendWebhooks::new(SECRET).unwrap();
        assert_eq!(webhooks.sign(ID, TIMESTAMP, BODY), SIGNATURE);
    }

    #[test]
    fn a_changed_body_id_or_timestamp_breaks_the_signature() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert_eq!(
            verify(&genuine, br#"{"test": 2432232315}"#, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
        let other_id = headers("msg_other", TIMESTAMP, SIGNATURE);
        assert_eq!(
            verify(&other_id, BODY, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
        let other_time = headers(ID, TIMESTAMP + 1, SIGNATURE);
        assert_eq!(
            verify(&other_time, BODY, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
        let unversioned = headers(
            ID,
            TIMESTAMP,
            "g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=",
        );
        assert_eq!(
            verify(&unversioned, BODY, at(TIMESTAMP)),
            Err(WebhookError::BadSignature)
        );
    }

    #[test]
    fn replays_outside_five_minutes_and_unsigned_requests_are_refused() {
        let genuine = headers(ID, TIMESTAMP, SIGNATURE);
        assert!(verify(&genuine, BODY, at(TIMESTAMP + 300)).is_ok());
        assert_eq!(
            verify(&genuine, BODY, at(TIMESTAMP + 301)),
            Err(WebhookError::Stale)
        );
        assert_eq!(
            verify(&genuine, BODY, at(TIMESTAMP - 301)),
            Err(WebhookError::Stale)
        );
        let mut unsigned = genuine.clone();
        unsigned.remove("svix-signature");
        assert_eq!(
            verify(&unsigned, BODY, at(TIMESTAMP)),
            Err(WebhookError::MissingHeaders)
        );
    }

    #[test]
    fn secrets_must_be_base64_keys() {
        assert!(ResendWebhooks::new("whsec_not base64!").is_err());
        assert!(ResendWebhooks::new("whsec_AAAA").is_err());
        let shown = format!("{:?}", ResendWebhooks::new(SECRET).unwrap());
        assert!(!shown.contains("MfKQ"), "{shown}");
    }

    fn parse(event: Value) -> Result<Option<DeliveryEvent>, WebhookError> {
        ResendWebhooks::new(SECRET)
            .unwrap()
            .parse(event.to_string().as_bytes())
    }

    fn resend_event(kind: &str, data: Value) -> Value {
        let mut data = data;
        data["email_id"] = json!("56761188-7520-42d8-8898-ff6fc54ce618");
        data["to"] = json!(["delivered@resend.dev"]);
        json!({"type": kind, "created_at": "2026-11-22T23:41:12.126Z", "data": data})
    }

    #[test]
    fn bounces_complaints_and_deliveries_become_events() {
        let hard = parse(resend_event(
            "email.bounced",
            json!({"bounce": {"type": "Permanent", "subType": "Suppressed"}}),
        ))
        .unwrap()
        .unwrap();
        assert_eq!(hard.kind, DeliveryKind::Bounced { permanent: true });
        assert_eq!(
            hard.provider_message_id,
            "56761188-7520-42d8-8898-ff6fc54ce618"
        );
        assert_eq!(hard.recipients, ["delivered@resend.dev"]);
        assert_eq!(
            hard.occurred_at.unwrap().to_rfc3339(),
            "2026-11-22T23:41:12.126+00:00"
        );
        assert!(hard.suppresses());

        let soft = parse(resend_event(
            "email.bounced",
            json!({"bounce": {"type": "Transient"}}),
        ))
        .unwrap()
        .unwrap();
        assert!(!soft.suppresses());
        let complaint = parse(resend_event("email.complained", json!({})))
            .unwrap()
            .unwrap();
        assert!(complaint.suppresses());
        let delivered = parse(resend_event("email.delivered", json!({})))
            .unwrap()
            .unwrap();
        assert_eq!(delivered.kind, DeliveryKind::Delivered);
        assert!(!delivered.suppresses());
    }

    #[test]
    fn other_events_are_ignored_and_garbage_is_refused() {
        assert_eq!(parse(resend_event("email.opened", json!({}))), Ok(None));
        assert_eq!(parse(json!({"data": {}})), Err(WebhookError::Malformed));
        assert_eq!(
            parse(json!({"type": "email.bounced", "data": {}})),
            Err(WebhookError::Malformed)
        );
        let webhooks = ResendWebhooks::new(SECRET).unwrap();
        assert_eq!(webhooks.parse(b"not json"), Err(WebhookError::Malformed));
    }
}
