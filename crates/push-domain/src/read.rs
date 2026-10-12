use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{Conversation, ConversationKind, ConversationState, Member, MemberState};
use crate::notify::{Event, Notifier, ReceiptUpdate};
use crate::service::{Messaging, current_member_ids};
use crate::store::{Store, Tx};

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Records that `user` has read everything up to `seq`. Returns whether the position
    /// moved; it never moves backwards. When it moves, the user's devices learn the new
    /// position, the other member of a direct conversation gets a read receipt if the move
    /// passed over a message from them, and the members of a group get its new group-wide
    /// positions if they moved.
    pub async fn read_up_to(
        &self,
        user: UserId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;
        if seq < 0 || seq > member.visible_upper(found.last_seq) {
            return Err(DomainError::SeqOutOfRange);
        }
        let group_before = group_positions(&mut tx, &found, found.last_seq).await?;
        let moved = tx.raise_read_seq(conversation, user, seq).await?;
        // Messages below the member's floor were never visible to them, so they are never
        // reported as read.
        let read_before = member.read_seq.max(member.visible_from_seq - 1);
        let receipt = direct_receipt(&mut tx, &found, user, read_before, seq).await?;
        let group = group_receipt(&mut tx, &found, found.last_seq, group_before).await?;
        tx.commit().await?;

        let mut events = Vec::new();
        if moved {
            events.push(Event::ReadPositionChanged {
                user,
                conversation,
                read_seq: seq,
            });
        }
        events.extend(receipt);
        events.extend(group);
        self.publish(events).await;
        Ok(moved)
    }

    /// Delivery and read positions of every member, as seen by `user`.
    pub async fn conversation_state(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<ConversationState> {
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        require_member(&mut tx, conversation, user).await?;

        let mut members = Vec::new();
        for member in tx.members(conversation).await? {
            let delivered_seq = tx.delivered_seq(conversation, member.user_id).await?;
            members.push(MemberState {
                user_id: member.user_id,
                delivered_seq: delivered_seq.max(member.read_seq),
                read_seq: member.read_seq,
            });
        }
        tx.rollback().await?;
        Ok(ConversationState {
            conversation_id: conversation,
            last_seq: found.last_seq,
            members,
        })
    }
}

/// The receipt for the other members of a direct conversation after one of `user`'s
/// positions moved from `before` to `after` (spec §4.5). `None` unless the conversation is
/// direct and the move passed over at least one live message that someone else sent:
/// moving over edits, deletes, system entries or one's own messages tells nobody anything.
///
/// Reads both positions as this transaction sees them, so call it after the move. Reports
/// `delivered_seq` as at least `read_seq`, so read always implies delivered.
pub(crate) async fn direct_receipt<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    user: UserId,
    before: i64,
    after: i64,
) -> DomainResult<Option<Event>> {
    if conversation.kind != ConversationKind::Direct || after <= before {
        return Ok(None);
    }
    if tx
        .count_incoming_messages(conversation.id, user, before, after, 1)
        .await?
        == 0
    {
        return Ok(None);
    }
    let members = tx.members(conversation.id).await?;
    let read_seq = members
        .iter()
        .find(|member| member.user_id == user)
        .map(|member| member.read_seq)
        .ok_or(DomainError::NotMember)?;
    let to: Vec<UserId> = current_member_ids(&members)
        .into_iter()
        .filter(|member| *member != user)
        .collect();
    if to.is_empty() {
        return Ok(None);
    }
    let delivered_seq = tx.delivered_seq(conversation.id, user).await?;
    Ok(Some(Event::Receipt {
        conversation: conversation.id,
        to,
        receipt: ReceiptUpdate::Direct {
            user_id: user,
            delivered_seq: delivered_seq.max(read_seq),
            read_seq,
        },
    }))
}

/// Group-wide positions (spec §4.5): every live message up to `read` has been read, and
/// every one up to `delivered` delivered, by every current member other than its sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupPositions {
    pub(crate) delivered: i64,
    pub(crate) read: i64,
}

/// The group-wide positions of `conversation` with its log ending at `last_seq`, the
/// minimum over its current members of their effective positions. `None` for a direct
/// conversation or a group without members.
///
/// A member's effective position skips their own messages, edits, deletes and system
/// entries: it lies just before the first live message from someone else after their
/// position, within their visible range, or at the end of that range if there is none.
/// The delivered position is at least the read one, since reading implies delivery.
pub(crate) async fn group_positions<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
) -> DomainResult<Option<GroupPositions>> {
    if conversation.kind != ConversationKind::Group {
        return Ok(None);
    }
    let mut all: Option<GroupPositions> = None;
    for member in tx.members(conversation.id).await? {
        if !member.is_current() {
            continue;
        }
        let read = effective_position(tx, &member, last_seq, member.read_seq).await?;
        let delivered_seq = tx.delivered_seq(conversation.id, member.user_id).await?;
        let delivered = if delivered_seq > member.read_seq {
            effective_position(tx, &member, last_seq, delivered_seq)
                .await?
                .max(read)
        } else {
            read
        };
        all = Some(match all {
            None => GroupPositions { delivered, read },
            Some(other) => GroupPositions {
                delivered: other.delivered.min(delivered),
                read: other.read.min(read),
            },
        });
    }
    Ok(all)
}

/// How far `member`, at `position`, has effectively got: see [`group_positions`].
async fn effective_position<T: Tx>(
    tx: &mut T,
    member: &Member,
    last_seq: i64,
    position: i64,
) -> DomainResult<i64> {
    let upper = member.visible_upper(last_seq);
    let from = position.max(member.visible_from_seq - 1);
    if from >= upper {
        return Ok(upper);
    }
    let first = tx
        .first_live_message(member.conversation_id, from, upper, Some(member.user_id))
        .await?;
    Ok(first.map_or(upper, |seq| seq - 1))
}

/// The receipt for a group's current members after a change, if its group-wide positions
/// moved from `before` over at least one live message: moving only over edits, deletes or
/// system entries tells nobody anything (spec §4.5). `None` for a direct conversation.
///
/// Reads the positions as this transaction sees them, so call it after the change, with
/// the log's new end as `last_seq`.
pub(crate) async fn group_receipt<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
    before: Option<GroupPositions>,
) -> DomainResult<Option<Event>> {
    let Some(before) = before else {
        return Ok(None);
    };
    let Some(after) = group_positions(tx, conversation, last_seq).await? else {
        return Ok(None);
    };
    let mut news = false;
    for (from, to) in [
        (before.delivered, after.delivered),
        (before.read, after.read),
    ] {
        if !news && to > from {
            news = tx
                .first_live_message(conversation.id, from, to, None)
                .await?
                .is_some();
        }
    }
    if !news {
        return Ok(None);
    }
    let members = tx.members(conversation.id).await?;
    Ok(Some(Event::Receipt {
        conversation: conversation.id,
        to: current_member_ids(&members),
        receipt: ReceiptUpdate::Group {
            all_delivered_seq: after.delivered,
            all_read_seq: after.read,
        },
    }))
}
