//! Per-user hiding of direct conversations, and deleted accounts (spec
//! 2026-11-06-tombstones-hide-design.md).

use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::DomainResult;
use crate::ids::{ConversationId, UserId};
use crate::model::ConversationKind;
use crate::notify::{Event, Notifier};
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::ValidationError;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Hides a direct conversation from `user`'s own listings, on all of their devices;
    /// the other member's view is unchanged. A new message in it shows it again.
    /// Idempotent: hiding a hidden conversation changes and publishes nothing. A group is
    /// `Invalid(NotDirect)`.
    pub async fn hide_conversation(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<()> {
        self.set_hidden(user, conversation, true).await
    }

    /// Shows a hidden direct conversation again; as [`Self::hide_conversation`].
    pub async fn unhide_conversation(
        &self,
        user: UserId,
        conversation: ConversationId,
    ) -> DomainResult<()> {
        self.set_hidden(user, conversation, false).await
    }

    async fn set_hidden(
        &self,
        user: UserId,
        conversation: ConversationId,
        hidden: bool,
    ) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        require_member(&mut tx, conversation, user).await?;
        if found.kind != ConversationKind::Direct {
            return Err(ValidationError::NotDirect.into());
        }
        let changed = tx
            .set_conversation_hidden(conversation, user, hidden, self.clock.now())
            .await?;
        let peer_deleted = changed && peer_deleted(&mut tx, user, conversation).await?;
        tx.commit().await?;
        if changed {
            self.publish(vec![Event::ConversationFlags {
                user,
                conversation,
                hidden,
                peer_deleted,
            }])
            .await;
        }
        Ok(())
    }

    /// auth-svc deleted `user`'s account (`user.deleted`, relayed by the worker): records
    /// it once, keeping the first time, and tells the other member of each of `user`'s
    /// direct conversations that its peer is deleted. Records an unknown user too, so the
    /// relay is order-independent. A repeat changes and publishes nothing.
    pub async fn user_deleted(&self, user: UserId) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.lock_users(&[user]).await?;
        tx.insert_user(user).await?;
        if !tx.mark_user_deleted(user, self.clock.now()).await? {
            tx.rollback().await?;
            return Ok(());
        }
        let mut events = Vec::new();
        for (conversation, peer) in tx.direct_peers(user).await? {
            let hidden = tx
                .conversation_flags(peer)
                .await?
                .into_iter()
                .any(|flags| flags.conversation_id == conversation && flags.hidden);
            events.push(Event::ConversationFlags {
                user: peer,
                conversation,
                hidden,
                peer_deleted: true,
            });
        }
        tx.commit().await?;
        self.publish(events).await;
        Ok(())
    }
}

/// Whether the other member of `user`'s direct conversation is recorded deleted.
pub(crate) async fn peer_deleted<T: Tx>(
    tx: &mut T,
    user: UserId,
    conversation: ConversationId,
) -> DomainResult<bool> {
    Ok(tx
        .conversation_flags(user)
        .await?
        .into_iter()
        .any(|flags| flags.conversation_id == conversation && flags.peer_deleted))
}
