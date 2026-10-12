use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{Conversation, ConversationKind, Member, Role, direct_key};
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::ValidationError;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Returns the direct conversation between two accepted contacts, creating it if needed.
    pub async fn open_direct(&self, me: UserId, peer: UserId) -> DomainResult<ConversationId> {
        if me == peer {
            return Err(ValidationError::SelfTarget.into());
        }
        let key = direct_key(me, peer);
        let mut tx = self.store.begin().await?;
        tx.lock_contact_pair(me, peer).await?;
        if !are_contacts(&mut tx, me, peer).await? {
            return Err(DomainError::NotContact);
        }
        if let Some(existing) = tx.find_direct(key).await? {
            tx.rollback().await?;
            return Ok(existing);
        }

        let now = self.clock.now();
        let conversation = Conversation {
            id: ConversationId::new(),
            kind: ConversationKind::Direct,
            direct_key: Some(key),
            title: None,
            created_by: me,
            last_seq: 0,
            created_at: now,
            updated_at: now,
        };
        let id = conversation.id;
        if tx.insert_conversation(conversation).await? == InsertOutcome::Duplicate {
            // Safety net: the pair lock already serialises callers, but if a concurrent
            // request created it first, return that one.
            tx.rollback().await?;
            let mut tx = self.store.begin().await?;
            let existing = tx.find_direct(key).await?.ok_or_else(|| {
                DomainError::Store("direct conversation disappeared after a conflict".into())
            })?;
            tx.rollback().await?;
            return Ok(existing);
        }

        for user in [me, peer] {
            tx.insert_member(Member {
                conversation_id: id,
                user_id: user,
                role: Role::Member,
                visible_from_seq: 1,
                left_at_seq: None,
                read_seq: 0,
            })
            .await?;
            for device in tx.devices_of_user(user).await? {
                tx.create_cursor_if_absent(device, id, 0).await?;
            }
        }
        tx.commit().await?;
        Ok(id)
    }
}

pub(crate) async fn require_conversation<T: Tx>(
    tx: &mut T,
    id: ConversationId,
) -> DomainResult<Conversation> {
    tx.get_conversation(id)
        .await?
        .ok_or(DomainError::ConversationNotFound)
}

pub(crate) async fn require_member<T: Tx>(
    tx: &mut T,
    conversation: ConversationId,
    user: UserId,
) -> DomainResult<Member> {
    tx.get_member(conversation, user)
        .await?
        .ok_or(DomainError::NotMember)
}
