use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{Conversation, ConversationKind, ConversationState, MemberState};
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
    /// moved; it never moves backwards.
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
        let moved = tx.raise_read_seq(conversation, user, seq).await?;
        let receipt = direct_receipt(&mut tx, &found, user, member.read_seq, seq).await?;
        tx.commit().await?;
        self.publish(receipt.into_iter().collect()).await;
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
