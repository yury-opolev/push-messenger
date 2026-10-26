//! Presence: whether a user is online, as their accepted contacts may see it.
//!
//! The online state itself lives with the transport (it follows live connections); the
//! domain decides who may see it and stores the opt-out. Visibility needs an accepted
//! contact in both directions and neither side hiding presence: the opt-out is reciprocal,
//! so a user who hides cannot watch anyone else either.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::ContactState;
use crate::notify::{Event, Notifier};
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::ValidationError;

/// Most users one presence read may ask about.
pub const MAX_PRESENCE_BATCH: usize = 100;

/// What a viewer is shown. `Unknown` for everything not visible, so a stranger, a pending
/// request, a block either way, an opt-out and an id that does not exist all look alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceView {
    Unknown,
    Online,
    /// `last_seen` is the minute the user was last known online, when known.
    Offline {
        last_seen: Option<DateTime<Utc>>,
    },
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Which of `candidates` `viewer` may see the presence of: `viewer` itself always;
    /// others only if both contact rows are `Accepted` and neither the viewer nor the
    /// candidate hides presence (the opt-out is reciprocal). Sorted, deduplicated. An id
    /// that is not a user is simply not viewable, so the answer never reveals which ids
    /// exist. More than [`MAX_PRESENCE_BATCH`] distinct candidates, or none, is `Invalid`.
    pub async fn presence_viewable(
        &self,
        viewer: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>> {
        let mut candidates = candidates.to_vec();
        candidates.sort();
        candidates.dedup();
        if candidates.is_empty() || candidates.len() > MAX_PRESENCE_BATCH {
            return Err(ValidationError::PresenceBatch.into());
        }
        let mut tx = self.store.begin().await?;
        let mut viewable = Vec::new();
        if candidates.contains(&viewer) {
            viewable.push(viewer);
        }
        if !hides(&mut tx, viewer).await? {
            let others: HashSet<UserId> = candidates.into_iter().collect();
            viewable.extend(sharing_contacts(&mut tx, viewer, Some(&others)).await?);
        }
        tx.rollback().await?;
        viewable.sort();
        Ok(viewable)
    }

    /// Who hears `user`'s presence changes: accepted contacts (both rows `Accepted`) that
    /// do not hide presence; empty when `user` hides it. Sorted.
    pub async fn presence_audience(&self, user: UserId) -> DomainResult<Vec<UserId>> {
        let mut tx = self.store.begin().await?;
        let audience = audience(&mut tx, user).await?;
        tx.rollback().await?;
        Ok(audience)
    }

    /// Whether `user` shares presence. Sharing is the default.
    pub async fn presence_sharing(&self, user: UserId) -> DomainResult<bool> {
        let mut tx = self.store.begin().await?;
        let hidden = hides(&mut tx, user).await?;
        tx.rollback().await?;
        Ok(!hidden)
    }

    /// Switches sharing and returns the new setting. When it changes, hiding publishes
    /// `Unknown` to the former audience (computed before the change), and showing
    /// publishes `current()` (the transport's view of the user, read only when showing) to
    /// the new audience. When `current()` is `None` (the state cannot be read right now),
    /// showing still changes the setting but publishes nothing: a guess could push a wrong
    /// state, and the next announcement corrects the audience. An unchanged setting
    /// publishes nothing. An unknown user is `UnknownUser`.
    ///
    /// This takes the user's lock before it reads the setting. A show publishes before it
    /// commits, under the lock, so a later hide's `Unknown` cannot be overtaken by it;
    /// should the commit then fail, the audience sees the current state until the next
    /// announcement. A hide publishes `Unknown` after it commits, so a presence read that
    /// still saw the user as visible cannot be answered after it (see the re-check in the
    /// transport's presence read). An announcement cannot follow it with a stale state
    /// either: one under way when the hide took the lock published before, and one that
    /// takes the lock after the commit sees the hide (see [`Self::announce_presence`]).
    /// Only a show that follows a hide at once can be overtaken by the hide's `Unknown`,
    /// which hides the user until their next announcement (never a leak).
    pub async fn set_presence_sharing<F, Fut>(
        &self,
        user: UserId,
        share: bool,
        current: F,
    ) -> DomainResult<bool>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Option<PresenceView>> + Send,
    {
        let mut tx = self.store.begin().await?;
        tx.lock_users(&[user]).await?;
        if !tx.user_exists(user).await? {
            return Err(DomainError::UnknownUser);
        }
        if hides(&mut tx, user).await? != share {
            tx.rollback().await?;
            return Ok(share);
        }
        if share {
            tx.set_presence_hidden(user, false).await?;
            let to = audience(&mut tx, user).await?;
            if !to.is_empty()
                && let Some(view) = current().await
            {
                self.publish(vec![Event::Presence { to, user, view }]).await;
            }
            tx.commit().await?;
        } else {
            let former = audience(&mut tx, user).await?;
            tx.set_presence_hidden(user, true).await?;
            tx.commit().await?;
            if !former.is_empty() {
                self.publish(vec![Event::Presence {
                    to: former,
                    user,
                    view: PresenceView::Unknown,
                }])
                .await;
            }
        }
        Ok(share)
    }

    /// Publishes `current()` (the transport's view of `user`) to
    /// [`Self::presence_audience`]; nothing when it is empty, or when `current()` is `None`
    /// (the state cannot be read right now: a guess could push a wrong state).
    ///
    /// This takes the user's lock, computes the audience, reads `current()` and publishes,
    /// all before it releases the lock, so a hide or block (which takes the same lock) is
    /// never followed by a stale state: an announcement under way when it took the lock has
    /// already published, and one that takes the lock after it committed leaves the hidden
    /// or blocked contact out. Reading the state inside the lock also keeps two
    /// announcements in order: the later one publishes the later state. An unknown user takes a lock nobody else needs and has
    /// an empty audience.
    pub async fn announce_presence<F, Fut>(&self, user: UserId, current: F) -> DomainResult<()>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Option<PresenceView>> + Send,
    {
        let mut tx = self.store.begin().await?;
        tx.lock_users(&[user]).await?;
        let to = audience(&mut tx, user).await?;
        if !to.is_empty()
            && let Some(view) = current().await
        {
            self.publish(vec![Event::Presence { to, user, view }]).await;
        }
        tx.rollback().await?;
        Ok(())
    }
}

/// Whether `user` hides their presence.
async fn hides<T: Tx>(tx: &mut T, user: UserId) -> DomainResult<bool> {
    Ok(!tx.presence_hidden(&[user]).await?.is_empty())
}

/// See [`Messaging::presence_audience`].
async fn audience<T: Tx>(tx: &mut T, user: UserId) -> DomainResult<Vec<UserId>> {
    if hides(tx, user).await? {
        return Ok(Vec::new());
    }
    sharing_contacts(tx, user, None).await
}

/// `user`'s accepted contacts (both rows `Accepted`) who share presence, limited to
/// `among` when given. Sorted.
async fn sharing_contacts<T: Tx>(
    tx: &mut T,
    user: UserId,
    among: Option<&HashSet<UserId>>,
) -> DomainResult<Vec<UserId>> {
    let mut contacts = Vec::new();
    for (peer, state) in tx.contacts_of(user).await? {
        if state != ContactState::Accepted || among.is_some_and(|among| !among.contains(&peer)) {
            continue;
        }
        if tx.get_contact(peer, user).await? == Some(ContactState::Accepted) {
            contacts.push(peer);
        }
    }
    let hidden: HashSet<UserId> = tx.presence_hidden(&contacts).await?.into_iter().collect();
    contacts.retain(|peer| !hidden.contains(peer));
    contacts.sort();
    Ok(contacts)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::error::DomainError;
    use crate::ids::UserId;
    use crate::notify::Event;
    use crate::testing::{befriend, service, user};
    use crate::validation::ValidationError;

    fn sorted(mut users: Vec<UserId>) -> Vec<UserId> {
        users.sort();
        users
    }

    /// A state source for tests: the user is online.
    async fn online() -> Option<PresenceView> {
        Some(PresenceView::Online)
    }

    /// A state source for tests that cannot read the state.
    async fn unreadable() -> Option<PresenceView> {
        None
    }

    #[tokio::test]
    async fn an_announcement_publishes_what_the_state_source_reads_only_when_someone_hears() {
        let (service, notifier) = service();
        let (alice, bob, loner) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        notifier.take();
        let seen = PresenceView::Offline {
            last_seen: Some(Utc.with_ymd_and_hms(2026, 10, 26, 19, 0, 0).unwrap()),
        };
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let source = || async {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(seen)
        };

        service.announce_presence(alice, source).await.unwrap();
        assert_eq!(
            notifier.take(),
            vec![Event::Presence {
                to: vec![bob],
                user: alice,
                view: seen,
            }]
        );
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);

        service.announce_presence(loner, source).await.unwrap();
        service
            .announce_presence(UserId::new(), source)
            .await
            .unwrap();
        assert_eq!(notifier.take(), vec![]);
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "nobody hears it, so the state is not read"
        );
    }

    #[tokio::test]
    async fn presence_is_viewable_by_accepted_contacts_only() {
        let (service, _) = service();
        let me = user(&service).await;
        let (friend, stranger, asked, asking, blocked, blocker) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, me, friend).await;
        service.contact_request(me, asked).await.unwrap();
        service.contact_request(asking, me).await.unwrap();
        befriend(&service, me, blocked).await;
        service.contact_block(me, blocked).await.unwrap();
        befriend(&service, me, blocker).await;
        service.contact_block(blocker, me).await.unwrap();
        let unknown = UserId::new();

        let viewable = service
            .presence_viewable(
                me,
                &[
                    friend, stranger, asked, asking, blocked, blocker, unknown, me, friend,
                ],
            )
            .await
            .unwrap();

        assert_eq!(viewable, sorted(vec![me, friend]));
    }

    #[tokio::test]
    async fn hiding_presence_hides_it_from_contacts_and_hides_contacts_from_the_hider() {
        let (service, _) = service();
        let (alice, bob, carol) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        befriend(&service, alice, carol).await;
        befriend(&service, bob, carol).await;

        let sharing = service.set_presence_sharing(alice, false, online).await;

        assert_eq!(sharing, Ok(false));
        assert_eq!(service.presence_sharing(alice).await, Ok(false));
        assert_eq!(service.presence_sharing(bob).await, Ok(true));
        assert_eq!(
            service.presence_viewable(bob, &[alice, carol]).await,
            Ok(vec![carol])
        );
        assert_eq!(
            service.presence_viewable(alice, &[alice, bob, carol]).await,
            Ok(vec![alice])
        );

        service
            .set_presence_sharing(alice, true, online)
            .await
            .unwrap();
        assert_eq!(
            service.presence_viewable(alice, &[bob, carol]).await,
            Ok(sorted(vec![bob, carol]))
        );
    }

    #[tokio::test]
    async fn a_presence_read_names_1_to_100_distinct_users() {
        let (service, _) = service();
        let alice = user(&service).await;
        let many: Vec<UserId> = (0..=MAX_PRESENCE_BATCH).map(|_| UserId::new()).collect();
        let mut repeated = vec![alice; MAX_PRESENCE_BATCH + 1];
        repeated.extend(&many[..MAX_PRESENCE_BATCH - 1]);

        let refused = Err(DomainError::Invalid(ValidationError::PresenceBatch));
        assert_eq!(service.presence_viewable(alice, &many).await, refused);
        assert_eq!(service.presence_viewable(alice, &[]).await, refused);
        assert!(service.presence_viewable(alice, &repeated).await.is_ok());
    }

    #[tokio::test]
    async fn the_audience_is_accepted_contacts_that_share() {
        let (service, _) = service();
        let me = user(&service).await;
        let (friend, hider, asked, blocked) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, me, friend).await;
        befriend(&service, me, hider).await;
        befriend(&service, me, blocked).await;
        service.contact_request(me, asked).await.unwrap();
        service.contact_block(blocked, me).await.unwrap();
        service
            .set_presence_sharing(hider, false, online)
            .await
            .unwrap();

        assert_eq!(service.presence_audience(me).await, Ok(vec![friend]));

        service
            .set_presence_sharing(me, false, online)
            .await
            .unwrap();
        assert_eq!(service.presence_audience(me).await, Ok(vec![]));
    }

    #[tokio::test]
    async fn hiding_publishes_unknown_to_the_former_audience_and_showing_publishes_the_current_state()
     {
        let (service, notifier) = service();
        let (alice, bob, carol) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        befriend(&service, alice, carol).await;
        notifier.take();
        let audience = sorted(vec![bob, carol]);

        service
            .set_presence_sharing(alice, false, online)
            .await
            .unwrap();
        assert_eq!(
            notifier.take(),
            vec![Event::Presence {
                to: audience.clone(),
                user: alice,
                view: PresenceView::Unknown,
            }]
        );

        assert_eq!(
            service.set_presence_sharing(alice, false, online).await,
            Ok(false)
        );
        assert_eq!(notifier.take(), vec![], "an unchanged setting says nothing");

        let current = PresenceView::Offline {
            last_seen: Some(Utc.with_ymd_and_hms(2026, 10, 26, 18, 0, 0).unwrap()),
        };
        assert_eq!(
            service
                .set_presence_sharing(alice, true, move || async move { Some(current) })
                .await,
            Ok(true)
        );
        assert_eq!(
            notifier.take(),
            vec![Event::Presence {
                to: audience,
                user: alice,
                view: current,
            }]
        );
    }

    #[tokio::test]
    async fn a_state_that_cannot_be_read_is_never_published() {
        let (service, notifier) = service();
        let (alice, bob) = (user(&service).await, user(&service).await);
        befriend(&service, alice, bob).await;
        service
            .set_presence_sharing(alice, false, online)
            .await
            .unwrap();
        notifier.take();

        assert_eq!(
            service.set_presence_sharing(alice, true, unreadable).await,
            Ok(true)
        );
        assert_eq!(notifier.take(), vec![], "showing publishes no guess");
        assert_eq!(service.presence_sharing(alice).await, Ok(true));
        assert_eq!(service.presence_audience(alice).await, Ok(vec![bob]));

        assert_eq!(service.announce_presence(alice, unreadable).await, Ok(()));
        assert_eq!(notifier.take(), vec![], "announcing publishes no guess");
    }

    #[tokio::test]
    async fn an_unknown_user_cannot_switch_sharing() {
        let (service, _) = service();

        let refused = service
            .set_presence_sharing(UserId::new(), false, online)
            .await;

        assert_eq!(refused, Err(DomainError::UnknownUser));
    }

    #[tokio::test]
    async fn an_announcement_reaches_the_audience_and_nobody_else() {
        let (service, notifier) = service();
        let (alice, bob, carol) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        service.contact_request(alice, carol).await.unwrap();
        notifier.take();

        service.announce_presence(alice, online).await.unwrap();
        assert_eq!(
            notifier.take(),
            vec![Event::Presence {
                to: vec![bob],
                user: alice,
                view: PresenceView::Online,
            }]
        );

        service
            .set_presence_sharing(bob, false, online)
            .await
            .unwrap();
        notifier.take();
        service.announce_presence(alice, online).await.unwrap();
        assert_eq!(notifier.take(), vec![], "nobody may hear it");
    }

    #[tokio::test]
    async fn blocking_an_accepted_contact_publishes_unknown_both_ways() {
        let (service, notifier) = service();
        let (alice, bob, carol) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        notifier.take();

        service.contact_block(alice, bob).await.unwrap();

        let presence: Vec<Event> = notifier
            .take()
            .into_iter()
            .filter(|event| matches!(event, Event::Presence { .. }))
            .collect();
        assert_eq!(
            presence,
            vec![
                Event::Presence {
                    to: vec![bob],
                    user: alice,
                    view: PresenceView::Unknown,
                },
                Event::Presence {
                    to: vec![alice],
                    user: bob,
                    view: PresenceView::Unknown,
                },
            ]
        );

        service.contact_request(alice, carol).await.unwrap();
        notifier.take();
        service.contact_block(alice, carol).await.unwrap();
        assert!(
            !notifier
                .take()
                .iter()
                .any(|event| matches!(event, Event::Presence { .. })),
            "blocking a non-contact says nothing about presence"
        );
    }

    /// Checks, when an `Unknown` presence is published, that it comes after the commit:
    /// it begins its own transaction (which would wait on the in-memory store's lock while
    /// the publishing one is open) and records whether the recipients already lost sight.
    struct CommitProbe {
        store: crate::memory::InMemoryStore,
        hidden_already: std::sync::Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl Notifier for CommitProbe {
        async fn publish(&self, events: &[Event]) {
            for event in events {
                let Event::Presence {
                    to,
                    user,
                    view: PresenceView::Unknown,
                } = event
                else {
                    continue;
                };
                let mut tx =
                    tokio::time::timeout(std::time::Duration::from_secs(2), self.store.begin())
                        .await
                        .expect("published while the transaction was still open")
                        .unwrap();
                let audience = audience(&mut tx, *user).await.unwrap();
                tx.rollback().await.unwrap();
                self.hidden_already
                    .lock()
                    .unwrap()
                    .push(to.iter().all(|peer| !audience.contains(peer)));
            }
        }
    }

    #[tokio::test]
    async fn hiding_and_blocking_publish_unknown_only_after_the_commit() {
        let store = crate::memory::InMemoryStore::new();
        let probe = std::sync::Arc::new(CommitProbe {
            store: store.clone(),
            hidden_already: std::sync::Mutex::default(),
        });
        let service = Messaging::new(store, crate::clock::SystemClock, probe.clone());
        let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
        for (a, b) in [(alice, bob), (bob, carol)] {
            for user in [a, b] {
                service.register_user(user).await.unwrap();
            }
            service.contact_request(a, b).await.unwrap();
            service.contact_accept(b, a).await.unwrap();
        }

        service
            .set_presence_sharing(alice, false, online)
            .await
            .unwrap();
        assert_eq!(*probe.hidden_already.lock().unwrap(), [true]);

        service.contact_block(bob, carol).await.unwrap();
        assert_eq!(*probe.hidden_already.lock().unwrap(), [true, true, true]);
    }
}
