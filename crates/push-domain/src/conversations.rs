use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationOverview, ConversationSummary,
    Member, Receipts, Role, direct_key,
};
use crate::notify::Notifier;
use crate::read::visible_group_positions;
use crate::service::Messaging;
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

        tx.lock_users(&[me, peer]).await?;
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
    /// appended. Receipt positions (`receipts`) do not move
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
    ///
    /// A group `user` has left is listed as they last saw it: up to the entry at which they
    /// left, with its time as `updated_at`, and without members or admins. Its title and
    /// group-wide positions are current, the positions capped at where they left. Once no
    /// current members remain, the group-wide positions are reported as 0.
    pub async fn list_conversations(
        &self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
    ) -> DomainResult<Vec<ConversationSummary>> {
        let mut tx = self.store.begin().await?;
        let mut overviews = tx
            .conversation_overviews(user, changed_since, MAX_UNREAD)
            .await?;
        let blocked: HashSet<UserId> = tx
            .contacts_of(user)
            .await?
            .into_iter()
            .filter(|(_, state)| *state == ContactState::Blocked)
            .map(|(peer, _)| peer)
            .collect();
        tx.rollback().await?;
        overviews.sort_by_key(|overview| overview.conversation.id);

        let mut summaries = Vec::new();
        for overview in overviews {
            // Blocking hides a direct conversation from the blocker.
            if overview
                .peer
                .as_ref()
                .is_some_and(|(peer, _)| blocked.contains(&peer.user_id))
            {
                continue;
            }
            let ConversationOverview {
                conversation,
                member,
                members,
                admins,
                peer,
                unread,
                group,
                left_at,
            } = overview;
            let receipts = if conversation.kind == ConversationKind::Group {
                let group = visible_group_positions(group, &conversation, &member);
                Receipts::Group {
                    all_delivered_seq: group.delivered,
                    all_read_seq: group.read,
                }
            } else {
                let (peer_delivered_seq, peer_read_seq) = peer
                    .map_or((0, 0), |(peer, delivered_seq)| {
                        (delivered_seq.max(peer.read_seq), peer.read_seq)
                    });
                Receipts::Direct {
                    peer_delivered_seq,
                    peer_read_seq,
                }
            };
            let mut summary = ConversationSummary {
                conversation_id: conversation.id,
                kind: conversation.kind,
                title: conversation.title.clone(),
                last_seq: member.visible_upper(conversation.last_seq),
                read_seq: member.read_seq,
                unread,
                receipts,
                updated_at: conversation.updated_at,
                members,
                admins,
            };
            if member.left_at_seq.is_some() {
                // A former member sees the conversation as it was when they left.
                summary.updated_at = left_at.unwrap_or(conversation.created_at);
                if changed_since.is_some_and(|since| summary.updated_at <= since) {
                    continue;
                }
                summary.members.clear();
                summary.admins.clear();
            }
            summaries.push(summary);
        }
        Ok(summaries)
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
