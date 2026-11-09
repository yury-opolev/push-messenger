//! Publishing domain events to the gateways through Redis.

use async_trait::async_trait;
use prost::Message;
use push_domain::{Event, KickReason, Notifier, ReceiptUpdate};
use push_proto::internal::v1::{
    self as internal, ContactChanged, DirectReceipt, EntriesAvailable, GroupReceipt, Kick,
    Notification, ReadPositionChanged, ReceiptChanged, notification::Kind, receipt_changed,
};
use push_proto::v1::ActivityEvent;
use push_proto::{id_bytes, kick_channel, notify_channel};
use redis::aio::ConnectionManager;

use crate::calls::RedisCalls;
use crate::convert::{
    Viewer, activity_state_to_wire, agent_settings, contact_state, outgoing_action, presence,
};
use crate::presence::RedisPresence;

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

    /// The presence reader on this notifier's connection.
    pub fn presence(&self) -> RedisPresence {
        RedisPresence::new(self.connection.clone())
    }

    /// The call state store on this notifier's connection.
    pub fn calls(&self) -> RedisCalls {
        RedisCalls::new(self.connection.clone())
    }

    /// Publishes prepared `(channel, payload)` messages in one pipeline.
    pub(crate) async fn publish_raw(
        &self,
        messages: &[(String, Vec<u8>)],
    ) -> redis::RedisResult<()> {
        self.send(messages).await
    }

    async fn send(&self, messages: &[(String, Vec<u8>)]) -> redis::RedisResult<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut pipeline = redis::pipe();
        for (channel, payload) in messages {
            pipeline.publish(channel, payload).ignore();
        }
        // Sent again once if the connection had been closed. A notification is a doorbell
        // (the client syncs) and a kick names its reason, so one heard twice does no harm.
        let pipeline = &pipeline;
        push_credentials::retry_if_dropped(|| async move {
            pipeline
                .query_async::<()>(&mut self.connection.clone())
                .await
        })
        .await
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
            Event::Activity {
                to,
                conversation,
                user,
                state,
                label,
                ttl,
            } => {
                let event = ActivityEvent {
                    conversation_id: id_bytes(conversation.0),
                    user_id: id_bytes(user.0),
                    state: activity_state_to_wire(*state).into(),
                    label: label.clone(),
                    ttl_ms: u32::try_from(ttl.num_milliseconds().max(0)).unwrap_or(u32::MAX),
                };
                for recipient in to {
                    messages.push(message(recipient.0, Kind::ActivityChanged(event.clone())));
                }
            }
            Event::Presence { to, user, view } => {
                let presence = presence(*user, *view);
                for recipient in to {
                    messages.push(message(
                        recipient.0,
                        Kind::PresenceChanged(presence.clone()),
                    ));
                }
            }
            Event::ConversationFlags {
                user,
                conversation,
                hidden,
                peer_deleted,
            } => {
                let kind = Kind::ConversationFlags(push_proto::v1::ConversationFlags {
                    conversation_id: id_bytes(conversation.0),
                    hidden: *hidden,
                    peer_deleted: *peer_deleted,
                });
                messages.push(message(user.0, kind));
            }
            Event::AgentContact {
                owner,
                agent,
                user,
                state,
            } => {
                let kind = Kind::AgentContactChanged(push_proto::v1::AgentContactEvent {
                    agent_user_id: id_bytes(agent.0),
                    user_id: id_bytes(user.0),
                    state: contact_state(*state).into(),
                });
                messages.push(message(owner.0, kind));
            }
            Event::AgentSettings { agent, settings } => {
                let kind = Kind::AgentSettingsChanged(push_proto::v1::AgentSettingsEvent {
                    settings: Some(agent_settings(settings)),
                });
                messages.push(message(agent.0, kind));
            }
            Event::Outgoing { to, action } => {
                // The agent cannot tell an expired action from a declined one.
                let viewer = if *to == action.agent {
                    Viewer::Agent
                } else {
                    Viewer::Owner
                };
                let kind = Kind::OutgoingActionChanged(push_proto::v1::OutgoingActionEvent {
                    action: Some(outgoing_action(action, viewer)),
                });
                messages.push(message(to.0, kind));
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
    use push_domain::{
        ActivityState, AgentSettings, ContactState, ConversationId, DeviceId, InboundPolicy,
        OutgoingAction, OutgoingError, OutgoingKind, OutgoingPolicy, OutgoingState, PresenceView,
        UserId,
    };
    use push_proto::v1;

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
    fn agent_contact_changes_go_to_the_owner_only() {
        let (owner, agent, user) = (UserId::new(), UserId::new(), UserId::new());
        let messages = notifications(&[Event::AgentContact {
            owner,
            agent,
            user,
            state: Some(ContactState::RequestedIn),
        }]);
        assert_eq!(
            decode(&messages),
            vec![(
                notify_channel(owner.0),
                Kind::AgentContactChanged(v1::AgentContactEvent {
                    agent_user_id: id_bytes(agent.0),
                    user_id: id_bytes(user.0),
                    state: v1::ContactState::RequestedIn.into(),
                })
            )]
        );
    }

    #[test]
    fn settings_changes_go_to_the_agent_only() {
        let agent = UserId::new();
        let settings = AgentSettings {
            calls: OutgoingPolicy::Allowed,
            inbound: InboundPolicy::Contacts,
            ..AgentSettings::default()
        };
        let messages = notifications(&[Event::AgentSettings { agent, settings }]);
        assert_eq!(
            decode(&messages),
            vec![(
                notify_channel(agent.0),
                Kind::AgentSettingsChanged(v1::AgentSettingsEvent {
                    settings: Some(v1::AgentSettings {
                        inbound: v1::InboundPolicy::Contacts.into(),
                        requests: v1::OutgoingPolicy::Ask.into(),
                        chats: v1::OutgoingPolicy::Ask.into(),
                        groups: v1::OutgoingPolicy::Off.into(),
                        calls: v1::OutgoingPolicy::Allowed.into(),
                    }),
                })
            )]
        );
    }

    #[test]
    fn outgoing_actions_go_to_their_recipient_and_the_agent_never_sees_expired() {
        let (owner, agent, peer) = (UserId::new(), UserId::new(), UserId::new());
        let at = chrono::Utc::now();
        let action = |state, error| OutgoingAction {
            action_id: uuid::Uuid::now_v7(),
            agent,
            kind: OutgoingKind::Chat,
            target: Some(peer),
            conversation: None,
            title: None,
            members: Vec::new(),
            state,
            error,
            created_at: at,
            resolved_at: Some(at),
            used_at: None,
        };
        let state_of = |to: UserId, action: &OutgoingAction| {
            let messages = notifications(&[Event::Outgoing {
                to,
                action: action.clone(),
            }]);
            let decoded = decode(&messages);
            assert_eq!(decoded.len(), 1);
            assert_eq!(decoded[0].0, notify_channel(to.0));
            let Kind::OutgoingActionChanged(v1::OutgoingActionEvent { action: Some(wire) }) =
                &decoded[0].1
            else {
                panic!("not an outgoing action: {decoded:?}");
            };
            assert_eq!(wire.action_id, id_bytes(action.action_id));
            assert_eq!(wire.agent_user_id, id_bytes(agent.0));
            assert_eq!(wire.target_user_id, id_bytes(peer.0));
            assert_eq!(wire.kind(), v1::OutgoingKind::Chat);
            (wire.state(), wire.error())
        };
        let expired = action(OutgoingState::Expired, None);
        assert_eq!(
            state_of(owner, &expired),
            (v1::OutgoingState::Expired, v1::ErrorCode::Unspecified)
        );
        assert_eq!(
            state_of(agent, &expired).0,
            v1::OutgoingState::OwnerDeclined
        );
        let declined = action(OutgoingState::Declined, None);
        for to in [owner, agent] {
            assert_eq!(state_of(to, &declined).0, v1::OutgoingState::OwnerDeclined);
        }
        let waiting = action(OutgoingState::Waiting, None);
        assert_eq!(state_of(owner, &waiting).0, v1::OutgoingState::Waiting);
        let failed = action(OutgoingState::Failed, Some(OutgoingError::NotContact));
        for to in [owner, agent] {
            assert_eq!(
                state_of(to, &failed),
                (v1::OutgoingState::Failed, v1::ErrorCode::NotContact)
            );
        }
        let failed = action(OutgoingState::Failed, Some(OutgoingError::Failed));
        assert_eq!(state_of(agent, &failed).1, v1::ErrorCode::Rejected);
    }

    #[test]
    fn conversation_flags_go_to_their_user_only() {
        let alice = UserId::new();
        let conversation = ConversationId::new();
        let messages = notifications(&[Event::ConversationFlags {
            user: alice,
            conversation,
            hidden: true,
            peer_deleted: true,
        }]);
        assert_eq!(
            decode(&messages),
            vec![(
                notify_channel(alice.0),
                Kind::ConversationFlags(v1::ConversationFlags {
                    conversation_id: id_bytes(conversation.0),
                    hidden: true,
                    peer_deleted: true,
                })
            )]
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
    fn an_activity_goes_to_each_recipient_with_its_fields() {
        let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
        let conversation = ConversationId::new();
        let messages = notifications(&[
            Event::Activity {
                to: vec![bob, carol],
                conversation,
                user: alice,
                state: ActivityState::Working,
                label: Some("running tests".to_owned()),
                ttl: push_domain::ACTIVITY_TTL,
            },
            Event::Activity {
                to: vec![bob],
                conversation,
                user: alice,
                state: ActivityState::Idle,
                label: None,
                ttl: chrono::TimeDelta::zero(),
            },
        ]);
        let working = Kind::ActivityChanged(v1::ActivityEvent {
            conversation_id: id_bytes(conversation.0),
            user_id: id_bytes(alice.0),
            state: v1::ActivityState::Working.into(),
            label: Some("running tests".to_owned()),
            ttl_ms: 30_000,
        });
        let idle = Kind::ActivityChanged(v1::ActivityEvent {
            conversation_id: id_bytes(conversation.0),
            user_id: id_bytes(alice.0),
            state: v1::ActivityState::Idle.into(),
            label: None,
            ttl_ms: 0,
        });
        assert_eq!(
            decode(&messages),
            vec![
                (notify_channel(bob.0), working.clone()),
                (notify_channel(carol.0), working),
                (notify_channel(bob.0), idle),
            ]
        );
    }

    #[test]
    fn a_presence_change_goes_to_each_recipient() {
        let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
        let minute = chrono::DateTime::from_timestamp(1_760_000_040, 0).unwrap();
        let messages = notifications(&[
            Event::Presence {
                to: vec![bob, carol],
                user: alice,
                view: PresenceView::Online,
            },
            Event::Presence {
                to: vec![bob],
                user: alice,
                view: PresenceView::Offline {
                    last_seen: Some(minute),
                },
            },
            Event::Presence {
                to: vec![carol],
                user: alice,
                view: PresenceView::Unknown,
            },
        ]);
        let presence = |state: v1::PresenceState, last_seen| {
            Kind::PresenceChanged(v1::Presence {
                user_id: id_bytes(alice.0),
                state: state.into(),
                last_seen,
            })
        };
        assert_eq!(
            decode(&messages),
            vec![
                (
                    notify_channel(bob.0),
                    presence(v1::PresenceState::Online, None)
                ),
                (
                    notify_channel(carol.0),
                    presence(v1::PresenceState::Online, None)
                ),
                (
                    notify_channel(bob.0),
                    presence(v1::PresenceState::Offline, Some(1_760_000_040_000_000))
                ),
                (
                    notify_channel(carol.0),
                    presence(v1::PresenceState::Unknown, None)
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
