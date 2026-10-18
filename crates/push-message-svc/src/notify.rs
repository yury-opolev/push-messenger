//! Publishing domain events to the gateways through Redis.

use async_trait::async_trait;
use prost::Message;
use push_domain::{Event, KickReason, Notifier, ReceiptUpdate};
use push_proto::internal::v1::{
    self as internal, ContactChanged, DirectReceipt, EntriesAvailable, GroupReceipt, Kick,
    Notification, ReadPositionChanged, ReceiptChanged, notification::Kind, receipt_changed,
};
use push_proto::{id_bytes, kick_channel, notify_channel};
use redis::aio::ConnectionManager;

use crate::convert::contact_state;

/// Publishes each event as small [`Notification`]s on the Redis channels of the users it
/// concerns (`notify:{user_id}`), and kicks as [`Kick`]s on the channels of the devices
/// (`kick:{device_id}`). Never a message body: a gateway that hears
/// `EntriesAvailable` syncs the user's devices.
///
/// Best effort, as `Notifier` requires: a failed publish is logged and dropped, since
/// clients catch up on their next sync. A failed kick is reported instead, so the relay
/// that asked for it is retried.
#[derive(Clone)]
pub struct RedisNotifier {
    connection: ConnectionManager,
}

impl RedisNotifier {
    /// Connects to Redis at `url` (`redis://:password@host:6379`). The connection
    /// reconnects by itself when it breaks.
    pub async fn connect(url: &str) -> redis::RedisResult<Self> {
        Self::with_connector(&push_credentials::RedisConnector::password(url)?).await
    }

    /// Connects through `connector` (password or Entra ID); reconnects by itself.
    pub async fn with_connector(
        connector: &push_credentials::RedisConnector,
    ) -> redis::RedisResult<Self> {
        let connection = connector
            .connection_manager(redis::aio::ConnectionManagerConfig::new())
            .await?;
        Ok(Self { connection })
    }

    async fn send(&self, messages: &[(String, Vec<u8>)]) -> redis::RedisResult<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut pipeline = redis::pipe();
        for (channel, payload) in messages {
            pipeline.publish(channel, payload).ignore();
        }
        let mut connection = self.connection.clone();
        pipeline.query_async::<()>(&mut connection).await
    }
}

#[async_trait]
impl Notifier for RedisNotifier {
    async fn publish(&self, events: &[Event]) {
        let messages = notifications(events);
        if let Err(error) = self.send(&messages).await {
            tracing::warn!(
                %error,
                count = messages.len(),
                "could not publish notifications; clients catch up on their next sync"
            );
        }
    }

    async fn kick(&self, events: &[Event]) -> Result<(), String> {
        let messages = notifications(events);
        self.send(&messages).await.map_err(|error| {
            tracing::warn!(
                %error,
                count = messages.len(),
                "could not publish kicks; the relay will retry"
            );
            error.to_string()
        })
    }
}

/// The channel and encoded [`Notification`] for every user each event concerns, in
/// event order.
pub(crate) fn notifications(events: &[Event]) -> Vec<(String, Vec<u8>)> {
    let mut messages = Vec::new();
    for event in events {
        match event {
            Event::EntriesAvailable { users } => {
                for user in users {
                    let kind = Kind::EntriesAvailable(EntriesAvailable {});
                    messages.push(message(user.0, kind));
                }
            }
            Event::Receipt {
                conversation,
                to,
                receipt,
            } => {
                let positions = match *receipt {
                    ReceiptUpdate::Direct {
                        user_id,
                        delivered_seq,
                        read_seq,
                    } => receipt_changed::Positions::Direct(DirectReceipt {
                        user_id: id_bytes(user_id.0),
                        delivered_seq,
                        read_seq,
                    }),
                    ReceiptUpdate::Group {
                        all_delivered_seq,
                        all_read_seq,
                    } => receipt_changed::Positions::Group(GroupReceipt {
                        all_delivered_seq,
                        all_read_seq,
                    }),
                    _ => continue,
                };
                for user in to {
                    let kind = Kind::ReceiptChanged(ReceiptChanged {
                        conversation_id: id_bytes(conversation.0),
                        positions: Some(positions.clone()),
                    });
                    messages.push(message(user.0, kind));
                }
            }
            Event::ContactChanged { user, peer, state } => {
                let kind = Kind::ContactChanged(ContactChanged {
                    peer_id: id_bytes(peer.0),
                    state: contact_state(*state).into(),
                });
                messages.push(message(user.0, kind));
            }
            Event::ReadPositionChanged {
                user,
                conversation,
                read_seq,
            } => {
                let kind = Kind::ReadPositionChanged(ReadPositionChanged {
                    conversation_id: id_bytes(conversation.0),
                    read_seq: *read_seq,
                });
                messages.push(message(user.0, kind));
            }
            Event::Kick { devices, reason } => {
                let reset_at = match reason {
                    KickReason::PasswordReset { reset_at } => reset_at.timestamp(),
                    KickReason::DeviceRevoked => 0,
                };
                let kick = Kick {
                    reason: kick_reason(*reason).into(),
                    reset_at,
                    connection_id: Vec::new(),
                };
                for device in devices {
                    messages.push((kick_channel(device.0), kick.encode_to_vec()));
                }
            }
            _ => {}
        }
    }
    messages
}

fn kick_reason(reason: KickReason) -> internal::KickReason {
    match reason {
        KickReason::DeviceRevoked => internal::KickReason::DeviceRevoked,
        KickReason::PasswordReset { .. } => internal::KickReason::PasswordReset,
    }
}

fn message(user: uuid::Uuid, kind: Kind) -> (String, Vec<u8>) {
    let notification = Notification { kind: Some(kind) };
    (notify_channel(user), notification.encode_to_vec())
}

#[cfg(test)]
mod tests {
    use push_domain::{ConversationId, DeviceId, UserId};

    use super::*;

    fn decode(messages: &[(String, Vec<u8>)]) -> Vec<(String, Kind)> {
        messages
            .iter()
            .map(|(channel, payload)| {
                let notification = Notification::decode(payload.as_slice()).unwrap();
                (channel.clone(), notification.kind.unwrap())
            })
            .collect()
    }

    #[test]
    fn new_entries_ring_every_member() {
        let (alice, bob) = (UserId::new(), UserId::new());
        let messages = notifications(&[Event::EntriesAvailable {
            users: vec![alice, bob],
        }]);
        assert_eq!(
            decode(&messages),
            vec![
                (
                    notify_channel(alice.0),
                    Kind::EntriesAvailable(EntriesAvailable {})
                ),
                (
                    notify_channel(bob.0),
                    Kind::EntriesAvailable(EntriesAvailable {})
                ),
            ]
        );
    }

    #[test]
    fn a_receipt_goes_to_every_recipient_with_its_positions() {
        let (alice, bob) = (UserId::new(), UserId::new());
        let conversation = ConversationId::new();
        let messages = notifications(&[Event::Receipt {
            conversation,
            to: vec![alice],
            receipt: ReceiptUpdate::Direct {
                user_id: bob,
                delivered_seq: 4,
                read_seq: 3,
            },
        }]);
        assert_eq!(
            decode(&messages),
            vec![(
                notify_channel(alice.0),
                Kind::ReceiptChanged(ReceiptChanged {
                    conversation_id: id_bytes(conversation.0),
                    positions: Some(receipt_changed::Positions::Direct(DirectReceipt {
                        user_id: id_bytes(bob.0),
                        delivered_seq: 4,
                        read_seq: 3,
                    })),
                })
            )]
        );
    }

    #[test]
    fn a_group_receipt_goes_to_every_recipient_as_aggregate_positions() {
        let (alice, bob) = (UserId::new(), UserId::new());
        let conversation = ConversationId::new();
        let group_receipt = Kind::ReceiptChanged(ReceiptChanged {
            conversation_id: id_bytes(conversation.0),
            positions: Some(receipt_changed::Positions::Group(GroupReceipt {
                all_delivered_seq: 7,
                all_read_seq: 5,
            })),
        });
        let messages = notifications(&[Event::Receipt {
            conversation,
            to: vec![alice, bob],
            receipt: ReceiptUpdate::Group {
                all_delivered_seq: 7,
                all_read_seq: 5,
            },
        }]);
        assert_eq!(
            decode(&messages),
            vec![
                (notify_channel(alice.0), group_receipt.clone()),
                (notify_channel(bob.0), group_receipt),
            ]
        );
    }

    #[test]
    fn contact_and_read_position_changes_go_to_their_user_only() {
        let (alice, bob) = (UserId::new(), UserId::new());
        let conversation = ConversationId::new();
        let messages = notifications(&[
            Event::ContactChanged {
                user: alice,
                peer: bob,
                state: None,
            },
            Event::ReadPositionChanged {
                user: alice,
                conversation,
                read_seq: 9,
            },
        ]);
        assert_eq!(
            decode(&messages),
            vec![
                (
                    notify_channel(alice.0),
                    Kind::ContactChanged(ContactChanged {
                        peer_id: id_bytes(bob.0),
                        state: push_proto::v1::ContactState::None.into(),
                    })
                ),
                (
                    notify_channel(alice.0),
                    Kind::ReadPositionChanged(ReadPositionChanged {
                        conversation_id: id_bytes(conversation.0),
                        read_seq: 9,
                    })
                ),
            ]
        );
    }

    #[test]
    fn no_events_publish_nothing() {
        assert!(notifications(&[]).is_empty());
    }

    #[test]
    fn a_kick_goes_to_each_device_with_its_reason() {
        let (phone, laptop) = (DeviceId::new(), DeviceId::new());
        let reset_at = chrono::DateTime::from_timestamp(1_760_000_000, 0).unwrap();
        let messages = notifications(&[
            Event::Kick {
                devices: vec![phone, laptop],
                reason: KickReason::PasswordReset { reset_at },
            },
            Event::Kick {
                devices: vec![phone],
                reason: KickReason::DeviceRevoked,
            },
        ]);
        let decoded: Vec<(String, internal::KickReason, i64)> = messages
            .iter()
            .map(|(channel, payload)| {
                let kick = Kick::decode(payload.as_slice()).unwrap();
                (channel.clone(), kick.reason(), kick.reset_at)
            })
            .collect();
        let reset = internal::KickReason::PasswordReset;
        assert_eq!(
            decoded,
            vec![
                (kick_channel(phone.0), reset, 1_760_000_000),
                (kick_channel(laptop.0), reset, 1_760_000_000),
                (
                    kick_channel(phone.0),
                    internal::KickReason::DeviceRevoked,
                    0
                ),
            ]
        );
    }
}
