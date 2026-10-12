use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{
    Conversation, ConversationKind, ConversationState, GroupPositions, Member, MemberState,
    Positions,
};
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
        if seq < member.visible_from_seq - 1 {
            return Ok(false);
        }
        let group_before = group_positions_before(&mut tx, &found, found.last_seq).await?;
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

    /// Positions in a conversation, as seen by `user`: both members' positions in a direct
    /// conversation, only the group-wide positions in a group.
    pub async fn conversation_state(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<ConversationState> {
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let caller = require_member(&mut tx, conversation, user).await?;

        let positions = if found.kind == ConversationKind::Group {
            let group = group_positions_for(&mut tx, &found, &caller).await?;
            Positions::Group {
                all_delivered_seq: group.delivered,
                all_read_seq: group.read,
            }
        } else {
            let mut members = Vec::new();
            for member in tx.members(conversation).await? {
                let delivered_seq = tx.delivered_seq(conversation, member.user_id).await?;
                members.push(MemberState {
                    user_id: member.user_id,
                    delivered_seq: delivered_seq.max(member.read_seq),
                    read_seq: member.read_seq,
                });
            }
            Positions::Members(members)
        };
        tx.rollback().await?;
        Ok(ConversationState {
            conversation_id: conversation,
            last_seq: caller.visible_upper(found.last_seq),
            positions,
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

/// The group-wide positions of `conversation` with its log ending at `last_seq` (see
/// [`Tx::group_positions`]). `None` for a direct conversation or a group without current
/// members.
pub(crate) async fn group_positions<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
) -> DomainResult<Option<GroupPositions>> {
    if conversation.kind != ConversationKind::Group {
        return Ok(None);
    }
    tx.group_positions(conversation.id, last_seq).await
}

/// The group-wide positions before a change to `conversation`, with its log ending at
/// `last_seq`, after taking its positions lock so that concurrent changes are compared
/// one after the other. `None`, without locking, for a direct conversation.
pub(crate) async fn group_positions_before<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
) -> DomainResult<Option<GroupPositions>> {
    if conversation.kind != ConversationKind::Group {
        return Ok(None);
    }
    tx.lock_conversation_positions(conversation.id).await?;
    group_positions(tx, conversation, last_seq).await
}

/// The group-wide positions of a group as `member` may see them: never above the end of
/// their visible range, and zero for a group without members.
pub(crate) async fn group_positions_for<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    member: &Member,
) -> DomainResult<GroupPositions> {
    let upper = member.visible_upper(conversation.last_seq);
    let all = group_positions(tx, conversation, conversation.last_seq).await?;
    Ok(all.map_or(
        GroupPositions {
            delivered: 0,
            read: 0,
        },
        |all| GroupPositions {
            delivered: all.delivered.min(upper),
            read: all.read.min(upper),
        },
    ))
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
