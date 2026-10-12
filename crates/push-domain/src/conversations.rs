use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationSummary, Member, Role, direct_key,
};
use crate::notify::Notifier;
use crate::service::{Messaging, current_member_ids};
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::{MAX_UNREAD, ValidationError};

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

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Every conversation `user` is or was a member of, with positions and unread counts,
    /// sorted by id. With `changed_since`, only those whose `updated_at` is later.
    ///
    /// `changed_since` reflects content changes only: a conversation created or an entry
    /// appended. Receipt positions (`peer_delivered_seq`, `peer_read_seq`) do not move
    /// `updated_at`, so the busy conversation row is not written on every ack and read. A
    /// reconnecting client refreshes its ticks with one listing without `changed_since`.
    ///
    /// Clients should pass as `changed_since` the time of their previous listing minus a
    /// safety window (a few seconds plus allowed clock drift): `updated_at` is stamped when
    /// a change starts, before it commits, so a change can commit after a listing that ran
    /// later than its stamp.
    ///
    /// A direct conversation with someone `user` has blocked is left out: blocking hides it
    /// from the blocker (spec §5.5). The blocked user still sees it.
    pub async fn list_conversations(
        &self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
    ) -> DomainResult<Vec<ConversationSummary>> {
        let mut tx = self.store.begin().await?;
        let mut memberships = tx.memberships(user).await?;
        memberships.sort_by_key(|(conversation, _)| conversation.id);

        let mut summaries = Vec::new();
        for (conversation, member) in memberships {
            if changed_since.is_some_and(|since| conversation.updated_at <= since) {
                continue;
            }
            let members = tx.members(conversation.id).await?;
            if conversation.kind == ConversationKind::Direct
                && has_blocked_a_member(&mut tx, user, &members).await?
            {
                continue;
            }
            let last_seq = member.visible_upper(conversation.last_seq);
            let unread_after = member.read_seq.max(member.visible_from_seq - 1);
            let unread = tx
                .count_incoming_messages(conversation.id, user, unread_after, last_seq, MAX_UNREAD)
                .await?;
            let (peer_delivered_seq, peer_read_seq) =
                peer_positions(&mut tx, &conversation, user, &members).await?;
            summaries.push(ConversationSummary {
                conversation_id: conversation.id,
                kind: conversation.kind,
                title: conversation.title,
                last_seq,
                read_seq: member.read_seq,
                unread,
                peer_delivered_seq,
                peer_read_seq,
                updated_at: conversation.updated_at,
                members: current_member_ids(&members),
            });
        }
        tx.rollback().await?;
        Ok(summaries)
    }
}

/// How far the other member of a direct conversation got, as `(delivered, read)`, with
/// delivered at least read. `(0, 0)` for other conversations: Plan 2B adds group
/// aggregates.
async fn peer_positions<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    user: UserId,
    members: &[Member],
) -> DomainResult<(i64, i64)> {
    if conversation.kind != ConversationKind::Direct {
        return Ok((0, 0));
    }
    let Some(peer) = members.iter().find(|member| member.user_id != user) else {
        return Ok((0, 0));
    };
    let delivered_seq = tx.delivered_seq(conversation.id, peer.user_id).await?;
    Ok((delivered_seq.max(peer.read_seq), peer.read_seq))
}

/// Whether `user` has blocked any other member.
async fn has_blocked_a_member<T: Tx>(
    tx: &mut T,
    user: UserId,
    members: &[Member],
) -> DomainResult<bool> {
    for member in members.iter().filter(|member| member.user_id != user) {
        if tx.get_contact(user, member.user_id).await? == Some(ContactState::Blocked) {
            return Ok(true);
        }
    }
    Ok(false)
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
