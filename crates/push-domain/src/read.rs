use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{ConversationState, MemberState};
use crate::notify::Notifier;
use crate::service::Messaging;
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
        tx.commit().await?;
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
                delivered_seq,
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
