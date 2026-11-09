use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationFlags, ConversationKind, ConversationOverview,
    ConversationSummary, Member, Receipts, Role, direct_key,
};
use crate::notify::Notifier;
use crate::outgoing::{Ask, Gate, acting_agent, gate};
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
    ///
    /// An agent opening a new one with someone other than its owner passes its owner's
    /// outgoing policy first (spec 2026-11-09 proactive agents §3): `NotAllowedByOwner`,
    /// or `WaitingForOwner` with the waiting action. Such a chat is `started_by_agent`.
    pub async fn open_direct(&self, me: UserId, peer: UserId) -> DomainResult<ConversationId> {
        if me == peer {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        tx.lock_contact_pair(me, peer).await?;
        if let Some(existing) = existing_direct(&mut tx, me, peer).await? {
            tx.rollback().await?;
            return Ok(existing);
        }

        let now = self.clock.now();
        // Both users' locks in one call, before the gate takes the agent's again.
        tx.lock_users(&[me, peer]).await?;
        let by_agent = tx.agent_owner(me).await?.is_some_and(|owner| owner != peer);
        if by_agent {
            if let Some(owner) = acting_agent(&mut tx, me).await? {
                if let Gate::Waiting { action, new } =
                    gate(&mut tx, me, Ask::chat(peer), now).await?
                {
                    return self.wait_for_owner(tx, owner, action, new).await;
                }
            }
        }
        if let Some(id) = create_direct(&mut tx, me, peer, by_agent, now).await? {
            tx.commit().await?;
            return Ok(id);
        }
        // Safety net: the pair lock already serialises callers, but if a concurrent
        // request created it first, return that one.
        tx.rollback().await?;
        let mut tx = self.store.begin().await?;
        let existing = tx.find_direct(direct_key(me, peer)).await?.ok_or_else(|| {
            DomainError::Store("direct conversation disappeared after a conflict".into())
        })?;
        tx.rollback().await?;
        Ok(existing)
    }
}

/// The direct conversation of `me` and `peer`, if there is one; `NotContact` unless they
/// are accepted contacts. The caller holds their pair lock.
pub(crate) async fn existing_direct<T: Tx>(
    tx: &mut T,
    me: UserId,
    peer: UserId,
) -> DomainResult<Option<ConversationId>> {
    if !are_contacts(tx, me, peer).await? {
        return Err(DomainError::NotContact);
    }
    tx.find_direct(direct_key(me, peer)).await
}

/// Creates the direct conversation of `me` and `peer`, whose locks (pair and users) the
/// caller holds. `None` when one exists already (a concurrent insert won).
pub(crate) async fn create_direct<T: Tx>(
    tx: &mut T,
    me: UserId,
    peer: UserId,
    by_agent: bool,
    now: DateTime<Utc>,
) -> DomainResult<Option<ConversationId>> {
    let conversation = Conversation {
        id: ConversationId::new(),
        kind: ConversationKind::Direct,
        direct_key: Some(direct_key(me, peer)),
        title: None,
        created_by: me,
        last_seq: 0,
        created_at: now,
        updated_at: now,
        started_by_agent: by_agent,
    };
    let id = conversation.id;
    if tx.insert_conversation(conversation).await? == InsertOutcome::Duplicate {
        return Ok(None);
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
    Ok(Some(id))
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
    ///
    /// A direct conversation `user` hid is left out; see
    /// [`Self::list_conversations_with`] to list it too.
    pub async fn list_conversations(
        &self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
    ) -> DomainResult<Vec<ConversationSummary>> {
        self.list_conversations_with(user, changed_since, false)
            .await
    }

    /// [`Self::list_conversations`], also listing the direct conversations `user` hid when
    /// `include_hidden` is set. Every summary carries `hidden` and `peer_deleted`.
    ///
    /// A flag change (a hide or unhide, a new message un-hiding it, the peer's deletion)
    /// counts as a change for `changed_since`, though it does not move `updated_at`.
    pub async fn list_conversations_with(
        &self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
        include_hidden: bool,
    ) -> DomainResult<Vec<ConversationSummary>> {
        let mut tx = self.store.begin().await?;
        let flags: HashMap<ConversationId, ConversationFlags> = tx
            .conversation_flags(user)
            .await?
            .into_iter()
            .map(|flags| (flags.conversation_id, flags))
            .collect();
        let flagged_since = |id: &ConversationId, since: DateTime<Utc>| {
            flags.get(id).is_some_and(|flags| flags.changed_at > since)
        };
        // The store filters by content changes only; when a flag changed since, every
        // overview is read and filtered here (rare: flags change seldom).
        let flag_changed =
            changed_since.is_some_and(|since| flags.keys().any(|id| flagged_since(id, since)));
        let store_since = if flag_changed { None } else { changed_since };
        let mut overviews = tx
            .conversation_overviews(user, store_since, MAX_UNREAD)
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
            let id = overview.conversation.id;
            let (hidden, peer_deleted) = flags
                .get(&id)
                .map_or((false, false), |flags| (flags.hidden, flags.peer_deleted));
            if hidden && !include_hidden {
                continue;
            }
            if let Some(since) = changed_since.filter(|_| flag_changed) {
                let changed = if overview.member.left_at_seq.is_some() {
                    overview.left_at.unwrap_or(overview.conversation.created_at) > since
                } else {
                    overview.conversation.updated_at > since
                };
                if !changed && !flagged_since(&id, since) {
                    continue;
                }
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
                hidden,
                peer_deleted,
                started_by_agent: conversation.started_by_agent,
            };
            if member.left_at_seq.is_some() {
                // A former member sees the conversation as it was when they left.
                summary.updated_at = left_at.unwrap_or(conversation.created_at);
                if !flag_changed && changed_since.is_some_and(|since| summary.updated_at <= since) {
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
