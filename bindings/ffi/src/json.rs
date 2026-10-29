//! The data push-sdk's bindings hand to apps (spec §8.5), as JSON, in one place: the Node
//! binding gives these values to JavaScript as objects and the C binding as UTF-8 strings,
//! so both show the same fields and, above all, the same error codes as
//! [`SdkError::code`].
//!
//! Field names and enum values are camelCase (`clientMsgId`, `"requestedOut"`); error
//! codes keep the SDK's snake_case (`"wrong_key"`). Ids are UUID strings; times are
//! microseconds since the Unix epoch, as in the SDK (they stay below 2^53, so JavaScript
//! numbers hold them exactly), except fields named `…Ms` (activity and presence), which
//! are milliseconds. The schema is documented in `docs/sdk-bindings.md`.
//!
//! Nothing here logs. The database key never passes through JSON: the bindings take it as
//! bytes ([`config`]).

use std::str::FromStr;
use std::time::Duration;

use push_sdk::{
    Account, Activity, ActivityState, ConnectionState, Contact, ContactState, Conversation,
    ConversationKind, DatabaseKey, Device, Event, Invite, Message, MessageKind, MessageStatus,
    OutboxKind, Platform, Presence, PresenceState, Profile, SdkError,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// An error: `{"code", "message", "retryable", "retryAfterS"}`. `code` is
/// [`SdkError::code`], identical in Rust, Node and C.
pub fn error(error: &SdkError) -> Value {
    let retry_after_s = match error {
        SdkError::Server { retry_after_s, .. } => *retry_after_s,
        _ => None,
    };
    json!({
        "code": error.code(),
        "message": error.to_string(),
        "retryable": error.is_retryable(),
        "retryAfterS": retry_after_s,
    })
}

/// `{"state": "offline" | "connecting" | "connected" | "reconnecting" | "signedOut" |
/// "deviceRemoved" | "openedElsewhere"}`, with `attempt` and `retryInMs` while
/// reconnecting.
pub fn connection_state(state: ConnectionState) -> Value {
    let name = match state {
        ConnectionState::Offline => "offline",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting {
            attempt,
            retry_in_ms,
        } => {
            return json!({
                "state": "reconnecting",
                "attempt": attempt,
                "retryInMs": retry_in_ms,
            });
        }
        ConnectionState::SignedOut => "signedOut",
        ConnectionState::DeviceRemoved => "deviceRemoved",
        ConnectionState::OpenedElsewhere => "openedElsewhere",
    };
    json!({ "state": name })
}

pub fn account(account: &Account) -> Value {
    json!({
        "userId": account.user_id.to_string(),
        "deviceId": account.device_id.to_string(),
    })
}

pub fn message(message: &Message) -> Value {
    let kind = match message.kind {
        MessageKind::Text => "text",
        MessageKind::System => "system",
        MessageKind::Rich => "rich",
        MessageKind::Answer => "answer",
    };
    let status = match message.status {
        MessageStatus::Pending => "pending",
        MessageStatus::Sent => "sent",
        MessageStatus::Delivered => "delivered",
        MessageStatus::Read => "read",
        MessageStatus::Received => "received",
    };
    json!({
        "conversationId": message.conversation_id.to_string(),
        "clientMsgId": message.client_msg_id.to_string(),
        "messageId": message.message_id.map(|id| id.to_string()),
        "seq": message.seq,
        "senderId": message.sender_id.to_string(),
        "kind": kind,
        "body": message.body,
        "clientTs": message.client_ts,
        "serverTs": message.server_ts,
        "editedAt": message.edited_at,
        "deleted": message.deleted,
        "status": status,
        "answerSeq": message.answer_seq,
        "cardSeq": message.card_seq,
    })
}

pub fn conversation(conversation: &Conversation) -> Value {
    let kind = match conversation.kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Group => "group",
        ConversationKind::Unknown => "unknown",
    };
    json!({
        "id": conversation.id.to_string(),
        "kind": kind,
        "peer": conversation.peer.map(|peer| peer.to_string()),
        "title": conversation.title,
        "lastSeq": conversation.last_seq,
        "readSeq": conversation.read_seq,
        "unread": conversation.unread,
        "peerDeliveredSeq": conversation.peer_delivered_seq,
        "peerReadSeq": conversation.peer_read_seq,
        "updatedAt": conversation.updated_at,
    })
}

pub fn contact(contact: &Contact) -> Value {
    let state = match contact.state {
        ContactState::None => "none",
        ContactState::RequestedOut => "requestedOut",
        ContactState::RequestedIn => "requestedIn",
        ContactState::Accepted => "accepted",
        ContactState::Blocked => "blocked",
    };
    json!({ "userId": contact.user_id.to_string(), "state": state })
}

pub fn invite(invite: &Invite) -> Value {
    json!({
        "code": invite.code,
        "expiresAt": invite.expires_at,
        "maxUses": invite.max_uses,
        "uses": invite.uses,
    })
}

pub fn profile(profile: &Profile) -> Value {
    json!({
        "userId": profile.user_id.to_string(),
        "handle": profile.handle,
        "displayName": profile.display_name,
        "kind": profile.kind,
        "ownerUserId": profile.owner_user_id.map(|id| id.to_string()),
    })
}

pub fn device(device: &Device) -> Value {
    json!({
        "deviceId": device.device_id.to_string(),
        "name": device.name,
        "platform": device.platform,
        "createdAt": device.created_at,
        "lastSeenAt": device.last_seen_at,
        "current": device.current,
    })
}

/// Another member's live activity: `{conversationId, userId, state, label, expiresAtMs}`.
/// `expiresAtMs` is milliseconds since the Unix epoch on this device's clock.
pub fn activity(activity: &Activity) -> Value {
    let state = match activity.state {
        ActivityState::Typing => "typing",
        ActivityState::Working => "working",
        ActivityState::Waiting => "waiting",
        ActivityState::Idle => "idle",
    };
    json!({
        "conversationId": activity.conversation_id.to_string(),
        "userId": activity.user_id.to_string(),
        "state": state,
        "label": activity.label,
        "expiresAtMs": activity.expires_at_ms,
    })
}

/// `{userId, state, lastSeenMs}`: `state` is `unknown`, `online` or `offline`;
/// `lastSeenMs` (offline only, else `null`) is milliseconds since the Unix epoch.
pub fn presence(presence: &Presence) -> Value {
    let state = match presence.state {
        PresenceState::Unknown => "unknown",
        PresenceState::Online => "online",
        PresenceState::Offline => "offline",
    };
    json!({
        "userId": presence.user_id.to_string(),
        "state": state,
        "lastSeenMs": presence.last_seen_ms,
    })
}

/// Each of `items` with `shape`, as an array.
pub fn list<T>(items: &[T], shape: impl Fn(&T) -> Value) -> Value {
    Value::Array(items.iter().map(shape).collect())
}

/// An event: `{"type": …}` and its fields (see `docs/sdk-bindings.md`).
pub fn event(event: &Event) -> Value {
    match event {
        Event::ConnectionStateChanged(state) => {
            let mut value = connection_state(*state);
            value["type"] = json!("connectionStateChanged");
            value
        }
        Event::MessageAdded(added) => json!({"type": "messageAdded", "message": message(added)}),
        Event::MessageSent(sent) => json!({"type": "messageSent", "message": message(sent)}),
        Event::MessageEdited(edited) => {
            json!({"type": "messageEdited", "message": message(edited)})
        }
        Event::MessageDeleted(deleted) => {
            json!({"type": "messageDeleted", "message": message(deleted)})
        }
        Event::SendFailed {
            conversation_id,
            client_msg_id,
            kind,
            error: failure,
        } => {
            let kind = match kind {
                OutboxKind::Send => "send",
                OutboxKind::Edit => "edit",
                OutboxKind::Delete => "delete",
            };
            json!({
                "type": "sendFailed",
                "conversationId": conversation_id.to_string(),
                "clientMsgId": client_msg_id.to_string(),
                "kind": kind,
                "error": error(failure),
            })
        }
        Event::ReceiptChanged {
            conversation_id,
            peer_delivered_seq,
            peer_read_seq,
        } => json!({
            "type": "receiptChanged",
            "conversationId": conversation_id.to_string(),
            "peerDeliveredSeq": peer_delivered_seq,
            "peerReadSeq": peer_read_seq,
        }),
        Event::ConversationChanged(changed) => {
            json!({"type": "conversationChanged", "conversation": conversation(changed)})
        }
        Event::ContactChanged(changed) => {
            json!({"type": "contactChanged", "contact": contact(changed)})
        }
        Event::SignedOut => json!({"type": "signedOut"}),
        Event::DeviceRemoved => json!({"type": "deviceRemoved"}),
        Event::OpenedElsewhere => json!({"type": "openedElsewhere"}),
        Event::EventsLost => json!({"type": "eventsLost"}),
        Event::ActivityChanged(changed) => {
            json!({"type": "activityChanged", "activity": activity(changed)})
        }
        Event::PresenceChanged(changed) => {
            json!({"type": "presenceChanged", "presence": presence(changed)})
        }
    }
}

/// The settings an app opens a client with, as the bindings receive them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigFields {
    auth_url: String,
    socket_url: String,
    database_path: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    request_timeout_ms: Option<u64>,
    #[serde(default)]
    allow_insecure_transport: bool,
}

/// A [`push_sdk::Config`] from `{"authUrl", "socketUrl", "databasePath", "host"?,
/// "requestTimeoutMs"?, "allowInsecureTransport"?}` and, apart from the JSON so it never
/// sits in a string, the database key's 32 bytes. An unknown field is refused, so a
/// misspelt setting is not silently dropped.
pub fn config(value: Value, database_key: Option<&[u8]>) -> Result<push_sdk::Config, SdkError> {
    let fields: ConfigFields = serde_json::from_value(value)
        .map_err(|error| SdkError::Invalid(format!("config: {error}")))?;
    let mut config =
        push_sdk::Config::new(&fields.auth_url, &fields.socket_url, &fields.database_path);
    config.host = fields.host.filter(|host| !host.is_empty());
    if let Some(timeout) = fields.request_timeout_ms {
        config.request_timeout = Duration::from_millis(timeout);
    }
    config.allow_insecure_transport = fields.allow_insecure_transport;
    config.database_key = database_key.map(DatabaseKey::from_bytes).transpose()?;
    Ok(config)
}

/// `"ios"`, `"android"`, `"web"`, `"desktop"` or `"other"`.
pub fn platform(name: &str) -> Result<Platform, SdkError> {
    Ok(match name {
        "ios" => Platform::Ios,
        "android" => Platform::Android,
        "web" => Platform::Web,
        "desktop" => Platform::Desktop,
        "other" => Platform::Other,
        _ => return Err(SdkError::Invalid(format!("unknown platform {name:?}"))),
    })
}

/// `"typing"`, `"working"`, `"waiting"` or `"idle"`.
pub fn activity_state(name: &str) -> Result<ActivityState, SdkError> {
    Ok(match name {
        "typing" => ActivityState::Typing,
        "working" => ActivityState::Working,
        "waiting" => ActivityState::Waiting,
        "idle" => ActivityState::Idle,
        _ => {
            return Err(SdkError::Invalid(format!(
                "unknown activity state {name:?}"
            )));
        }
    })
}

/// An id from its UUID string; `invalid` naming `what` otherwise.
pub fn id<T: FromStr>(text: &str, what: &str) -> Result<T, SdkError> {
    text.parse()
        .map_err(|_| SdkError::Invalid(format!("{what} is not an id")))
}

#[cfg(test)]
mod tests {
    use push_sdk::{ClientMsgId, ConversationId, DeviceId, UserId};

    use super::*;

    #[test]
    fn errors_carry_the_sdk_codes_unchanged() {
        let cases = [
            SdkError::Timeout,
            SdkError::UnexpectedAnswer("x".to_owned()),
            SdkError::ConnectionBusy,
            SdkError::EncryptionUnavailable,
            SdkError::WrongKey,
            SdkError::Offline,
            SdkError::Invalid("empty".to_owned()),
            SdkError::Server {
                code: "rate_limited".to_owned(),
                message: "slow down".to_owned(),
                retryable: true,
                retry_after_s: Some(7),
            },
        ];
        for case in &cases {
            let shown = error(case);
            assert_eq!(shown["code"], case.code(), "{case:?}");
            assert_eq!(shown["retryable"], case.is_retryable(), "{case:?}");
        }
        assert_eq!(error(&cases[0])["code"], "timeout");
        assert_eq!(error(&cases[0])["retryable"], false);
        assert_eq!(error(&cases[1])["retryable"], false);
        assert_eq!(error(&cases[2])["retryable"], true);
        assert_eq!(error(&cases[3])["code"], "encryption_unavailable");
        assert_eq!(error(&cases[4])["code"], "wrong_key");
        assert_eq!(error(&cases[7])["retryAfterS"], 7);
        assert_eq!(error(&cases[0])["retryAfterS"], Value::Null);
    }

    #[test]
    fn events_have_a_type_and_camel_case_fields() {
        let state = event(&Event::ConnectionStateChanged(
            ConnectionState::Reconnecting {
                attempt: 2,
                retry_in_ms: 1500,
            },
        ));
        assert_eq!(
            state,
            json!({"type": "connectionStateChanged", "state": "reconnecting", "attempt": 2, "retryInMs": 1500})
        );
        let (conversation_id, client_msg_id) = (ConversationId::new(), ClientMsgId::new());
        let failed = event(&Event::SendFailed {
            conversation_id,
            client_msg_id,
            kind: OutboxKind::Edit,
            error: SdkError::Server {
                code: "not_contact".to_owned(),
                message: "not a contact".to_owned(),
                retryable: false,
                retry_after_s: None,
            },
        });
        assert_eq!(failed["type"], "sendFailed");
        assert_eq!(failed["clientMsgId"], client_msg_id.to_string());
        assert_eq!(failed["kind"], "edit");
        assert_eq!(failed["error"]["code"], "not_contact");
        assert_eq!(event(&Event::EventsLost), json!({"type": "eventsLost"}));
        let sender = UserId::new();
        let text = Message {
            conversation_id,
            client_msg_id,
            message_id: None,
            seq: Some(4),
            sender_id: sender,
            kind: MessageKind::Text,
            body: Some("hi".to_owned()),
            client_ts: None,
            server_ts: Some(1_760_000_000_000_000),
            edited_at: None,
            deleted: false,
            status: MessageStatus::Received,
            answer_seq: None,
            card_seq: None,
        };
        let added = event(&Event::MessageAdded(text.clone()));
        assert_eq!(added["message"]["senderId"], sender.to_string());
        assert_eq!(added["message"]["serverTs"], 1_760_000_000_000_000_i64);
        assert_eq!(added["message"]["messageId"], Value::Null);
        assert_eq!(added["message"]["status"], "received");
        assert_eq!(added["message"]["answerSeq"], Value::Null);
        let answer = |kind, answer_seq, card_seq| {
            message(&Message {
                kind,
                answer_seq,
                card_seq,
                ..text.clone()
            })
        };
        let card = answer(MessageKind::Rich, Some(5), None);
        assert_eq!(
            (&card["kind"], &card["answerSeq"]),
            (&json!("rich"), &json!(5))
        );
        let reply = answer(MessageKind::Answer, None, Some(4));
        assert_eq!(
            (&reply["kind"], &reply["cardSeq"]),
            (&json!("answer"), &json!(4))
        );
        let account = account(&Account {
            user_id: sender,
            device_id: DeviceId::new(),
        });
        assert_eq!(account["userId"], sender.to_string());
    }

    #[test]
    fn activity_and_presence_events_carry_their_fields() {
        let (conversation_id, user_id) = (ConversationId::new(), UserId::new());
        let typing = Activity {
            conversation_id,
            user_id,
            state: ActivityState::Working,
            label: Some("Running tests".to_owned()),
            expires_at_ms: 1_760_000_008_000,
        };
        assert_eq!(
            event(&Event::ActivityChanged(typing.clone())),
            json!({"type": "activityChanged", "activity": {
                "conversationId": conversation_id.to_string(),
                "userId": user_id.to_string(),
                "state": "working",
                "label": "Running tests",
                "expiresAtMs": 1_760_000_008_000_i64,
            }})
        );
        let idle = activity(&Activity {
            state: ActivityState::Idle,
            label: None,
            ..typing
        });
        assert_eq!(
            (&idle["state"], &idle["label"]),
            (&json!("idle"), &Value::Null)
        );
        let offline = Presence {
            user_id,
            state: PresenceState::Offline,
            last_seen_ms: Some(1_760_000_040_000),
        };
        assert_eq!(
            event(&Event::PresenceChanged(offline)),
            json!({"type": "presenceChanged", "presence": {
                "userId": user_id.to_string(),
                "state": "offline",
                "lastSeenMs": 1_760_000_040_000_i64,
            }})
        );
        let unknown = presence(&Presence {
            state: PresenceState::Unknown,
            last_seen_ms: None,
            ..offline
        });
        assert_eq!(
            (&unknown["state"], &unknown["lastSeenMs"]),
            (&json!("unknown"), &Value::Null)
        );
        let online = PresenceState::Online;
        assert_eq!(
            presence(&Presence {
                state: online,
                ..offline
            })["state"],
            "online"
        );
        for (name, state) in [
            ("typing", ActivityState::Typing),
            ("working", ActivityState::Working),
            ("waiting", ActivityState::Waiting),
            ("idle", ActivityState::Idle),
        ] {
            assert_eq!(activity_state(name).unwrap(), state);
        }
        assert_eq!(activity_state("Typing").unwrap_err().code(), "invalid");
    }

    #[test]
    fn a_config_takes_known_fields_and_the_key_as_bytes() {
        let value = json!({
            "authUrl": "http://127.0.0.1:1",
            "socketUrl": "ws://127.0.0.1:1/v1/socket",
            "databasePath": ":memory:",
            "host": "push.local",
            "requestTimeoutMs": 2500,
        });
        let opened = config(value.clone(), Some(&[7; 32])).unwrap();
        assert_eq!(opened.host.as_deref(), Some("push.local"));
        assert_eq!(opened.request_timeout, Duration::from_millis(2500));
        assert!(!opened.allow_insecure_transport);
        let mut insecure = value.clone();
        insecure["allowInsecureTransport"] = json!(true);
        assert!(config(insecure, None).unwrap().allow_insecure_transport);
        assert!(opened.database_key.is_some());
        assert_eq!(config(value.clone(), None).unwrap().database_key, None);
        assert_eq!(
            config(value.clone(), Some(&[7; 31])).unwrap_err().code(),
            "invalid"
        );
        let mut misspelt = value;
        misspelt["databaseKey"] = json!("00");
        assert_eq!(config(misspelt, None).unwrap_err().code(), "invalid");
        assert_eq!(platform("desktop").unwrap(), Platform::Desktop);
        assert_eq!(platform("toaster").unwrap_err().code(), "invalid");
        assert!(id::<UserId>("not an id", "userId").is_err());
    }

    /// docs/sdk-bindings.md tells app developers every code the SDK makes, whether it is
    /// retryable, and every event.
    #[test]
    fn the_bindings_guide_names_every_error_code_and_event() {
        let guide = include_str!("../../../docs/sdk-bindings.md");
        let text = String::new;
        let errors = [
            SdkError::Invalid(text()),
            SdkError::NotSignedIn,
            SdkError::DeviceRemoved,
            SdkError::OpenedElsewhere,
            SdkError::Offline,
            SdkError::Disconnected,
            SdkError::Timeout,
            SdkError::UnexpectedAnswer(text()),
            SdkError::ConnectionBusy,
            SdkError::Network(text()),
            SdkError::Storage(text()),
            SdkError::WrongKey,
            SdkError::EncryptionUnavailable,
            SdkError::Secrets(text()),
            SdkError::Signer(text()),
            SdkError::Closed,
        ];
        for failure in &errors {
            let retryable = if failure.is_retryable() { "yes" } else { "no" };
            let row = format!("| `{}` | {retryable} |", failure.code());
            assert!(guide.contains(&row), "{row}");
        }
        assert!(guide.contains("| `panic` | no |"));
        let (conversation_id, user) = (ConversationId::new(), UserId::new());
        let message = Message {
            conversation_id,
            client_msg_id: ClientMsgId::new(),
            message_id: None,
            seq: None,
            sender_id: user,
            kind: MessageKind::Text,
            body: None,
            client_ts: None,
            server_ts: None,
            edited_at: None,
            deleted: false,
            status: MessageStatus::Pending,
            answer_seq: None,
            card_seq: None,
        };
        let events = [
            Event::ConnectionStateChanged(ConnectionState::Offline),
            Event::MessageAdded(message.clone()),
            Event::MessageSent(message.clone()),
            Event::MessageEdited(message.clone()),
            Event::MessageDeleted(message),
            Event::SendFailed {
                conversation_id,
                client_msg_id: ClientMsgId::new(),
                kind: OutboxKind::Send,
                error: SdkError::Offline,
            },
            Event::ReceiptChanged {
                conversation_id,
                peer_delivered_seq: 0,
                peer_read_seq: 0,
            },
            Event::ConversationChanged(Conversation {
                id: conversation_id,
                kind: ConversationKind::Direct,
                peer: Some(user),
                title: None,
                last_seq: 0,
                read_seq: 0,
                unread: 0,
                peer_delivered_seq: 0,
                peer_read_seq: 0,
                updated_at: 0,
            }),
            Event::ContactChanged(Contact {
                user_id: user,
                state: ContactState::Accepted,
            }),
            Event::SignedOut,
            Event::DeviceRemoved,
            Event::OpenedElsewhere,
            Event::EventsLost,
            Event::ActivityChanged(Activity {
                conversation_id,
                user_id: user,
                state: ActivityState::Typing,
                label: None,
                expires_at_ms: 0,
            }),
            Event::PresenceChanged(Presence {
                user_id: user,
                state: PresenceState::Online,
                last_seen_ms: None,
            }),
        ];
        for shown in events.iter().map(event) {
            let row = format!("| `{}` |", shown["type"].as_str().unwrap());
            assert!(guide.contains(&row), "{row}");
        }
    }
}
