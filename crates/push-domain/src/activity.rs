//! Live activity: what a member is doing in a conversation right now (typing, working,
//! waiting, idle). Never stored; it lasts [`ACTIVITY_TTL`] unless sent again.

use chrono::TimeDelta;

use crate::clock::Clock;
use crate::conversations::require_conversation;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::ContactState;
use crate::notify::{Event, Notifier};
use crate::service::{Messaging, current_member_ids};
use crate::store::{Store, Tx};
use crate::templates::check_value;
use crate::validation::ValidationError;

/// How long an activity lasts unless sent again.
pub const ACTIVITY_TTL: TimeDelta = TimeDelta::seconds(30);
/// Longest activity label, in characters.
pub const MAX_ACTIVITY_LABEL_CHARS: usize = 60;

/// What a member is doing in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivityState {
    Typing,
    Working,
    /// Waiting for the others, for example while a card is open.
    Waiting,
    /// Nothing: clears the activity at once.
    Idle,
}

/// Checks a label: `None` or empty is no label (`Ok(None)`); anything else must keep 1 to
/// [`MAX_ACTIVITY_LABEL_CHARS`] characters, not only whitespace, and follow the rules of
/// template values ([`check_value`]): no control characters (no line breaks either), no
/// line or paragraph separators, no invisible, formatting or bidi characters. URLs and
/// markup are allowed: clients render the label as inert plain text.
pub fn validate_label(label: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(label) = label.filter(|label| !label.is_empty()) else {
        return Ok(None);
    };
    if label.trim().is_empty() {
        return Err(ValidationError::ActivityLabel(
            "is only whitespace".to_owned(),
        ));
    }
    check_value(label, MAX_ACTIVITY_LABEL_CHARS, false).map_err(ValidationError::ActivityLabel)?;
    Ok(Some(label.to_owned()))
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Publishes `sender`'s activity in `conversation` to the other current members that
    /// have no block with the sender in either direction, and returns them (sorted).
    /// Skipping a blocked member is silent, so an activity never reveals a block. Idle
    /// drops the label and has a zero ttl. A label is checked even for idle: bad input is
    /// refused whatever the state.
    ///
    /// An unknown conversation is `ConversationNotFound`; a sender who is not a current
    /// member is `NotMember`; a bad label is `Invalid`, before anything is read.
    pub async fn activity(
        &self,
        sender: UserId,
        conversation: ConversationId,
        state: ActivityState,
        label: Option<&str>,
    ) -> DomainResult<Vec<UserId>> {
        let label = validate_label(label)?;
        let (label, ttl) = match state {
            ActivityState::Idle => (None, TimeDelta::zero()),
            _ => (label, ACTIVITY_TTL),
        };
        let mut tx = self.store.begin().await?;
        require_conversation(&mut tx, conversation).await?;
        let members = tx.members(conversation).await?;
        let current = current_member_ids(&members);
        if !current.contains(&sender) {
            return Err(DomainError::NotMember);
        }
        let mut to = Vec::with_capacity(current.len() - 1);
        for member in current.into_iter().filter(|member| *member != sender) {
            if !blocked_either_way(&mut tx, sender, member).await? {
                to.push(member);
            }
        }
        tx.rollback().await?;
        if !to.is_empty() {
            self.publish(vec![Event::Activity {
                to: to.clone(),
                conversation,
                user: sender,
                state,
                label,
                ttl,
            }])
            .await;
        }
        Ok(to)
    }
}

/// Whether either user blocks the other.
async fn blocked_either_way<T: Tx>(tx: &mut T, a: UserId, b: UserId) -> DomainResult<bool> {
    let blocked = Some(ContactState::Blocked);
    Ok(tx.get_contact(a, b).await? == blocked || tx.get_contact(b, a).await? == blocked)
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::error::DomainError;
    use crate::ids::ConversationId;
    use crate::notify::Event;
    use crate::testing::{befriend, service, user};
    use crate::validation::ValidationError;

    #[tokio::test]
    async fn an_activity_goes_to_the_other_current_members_only() {
        let (service, notifier) = service();
        let (alice, bob, carol, dave) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        for other in [bob, carol, dave] {
            befriend(&service, alice, other).await;
        }
        let group = service
            .create_group(alice, "Team", &[bob, carol, dave])
            .await
            .unwrap();
        service.leave_group(dave, group).await.unwrap();
        notifier.take();

        let to = service
            .activity(alice, group, ActivityState::Working, Some("running tests"))
            .await
            .unwrap();

        let mut expected = vec![bob, carol];
        expected.sort();
        assert_eq!(to, expected);
        assert_eq!(
            notifier.take(),
            vec![Event::Activity {
                to: expected,
                conversation: group,
                user: alice,
                state: ActivityState::Working,
                label: Some("running tests".to_owned()),
                ttl: TimeDelta::seconds(30),
            }]
        );
    }

    #[tokio::test]
    async fn an_activity_in_a_conversation_the_sender_is_not_in_is_refused() {
        let (service, notifier) = service();
        let (alice, bob, carol) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        let direct = service.open_direct(alice, bob).await.unwrap();
        notifier.take();

        let refused = service
            .activity(carol, direct, ActivityState::Typing, None)
            .await;

        assert_eq!(refused, Err(DomainError::NotMember));
        assert_eq!(notifier.take(), vec![]);
    }

    #[tokio::test]
    async fn a_member_who_left_cannot_send_an_activity() {
        let (service, notifier) = service();
        let (alice, bob, carol) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        befriend(&service, alice, carol).await;
        let group = service
            .create_group(alice, "Team", &[bob, carol])
            .await
            .unwrap();
        service.leave_group(carol, group).await.unwrap();
        notifier.take();

        let refused = service
            .activity(carol, group, ActivityState::Typing, None)
            .await;

        assert_eq!(refused, Err(DomainError::NotMember));
        assert_eq!(notifier.take(), vec![]);
    }

    #[tokio::test]
    async fn an_unknown_conversation_is_not_found() {
        let (service, _) = service();
        let alice = user(&service).await;

        let refused = service
            .activity(alice, ConversationId::new(), ActivityState::Typing, None)
            .await;

        assert_eq!(refused, Err(DomainError::ConversationNotFound));
    }

    #[tokio::test]
    async fn members_with_a_block_either_way_are_skipped_silently() {
        let (service, notifier) = service();
        let (alice, bob, carol) = (
            user(&service).await,
            user(&service).await,
            user(&service).await,
        );
        befriend(&service, alice, bob).await;
        befriend(&service, alice, carol).await;
        let group = service
            .create_group(alice, "Team", &[bob, carol])
            .await
            .unwrap();
        service.contact_block(alice, carol).await.unwrap();
        notifier.take();

        let from_alice = service
            .activity(alice, group, ActivityState::Typing, None)
            .await;
        let from_carol = service
            .activity(carol, group, ActivityState::Typing, None)
            .await;

        assert_eq!(from_alice, Ok(vec![bob]));
        assert_eq!(from_carol, Ok(vec![bob]));
        let events = notifier.take();
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, Event::Activity { to, .. } if *to == vec![bob]))
        );
    }

    #[tokio::test]
    async fn nothing_is_published_when_nobody_may_hear_it() {
        let (service, notifier) = service();
        let (alice, bob) = (user(&service).await, user(&service).await);
        befriend(&service, alice, bob).await;
        let direct = service.open_direct(alice, bob).await.unwrap();
        service.contact_block(bob, alice).await.unwrap();
        notifier.take();

        let to = service
            .activity(alice, direct, ActivityState::Typing, None)
            .await;

        assert_eq!(to, Ok(vec![]));
        assert_eq!(notifier.take(), vec![]);
    }

    #[tokio::test]
    async fn idle_drops_the_label_and_has_no_ttl() {
        let (service, notifier) = service();
        let (alice, bob) = (user(&service).await, user(&service).await);
        befriend(&service, alice, bob).await;
        let direct = service.open_direct(alice, bob).await.unwrap();
        notifier.take();

        service
            .activity(alice, direct, ActivityState::Idle, Some("done"))
            .await
            .unwrap();

        assert_eq!(
            notifier.take(),
            vec![Event::Activity {
                to: vec![bob],
                conversation: direct,
                user: alice,
                state: ActivityState::Idle,
                label: None,
                ttl: TimeDelta::zero(),
            }]
        );
    }

    #[tokio::test]
    async fn an_invalid_label_is_refused_before_anything_is_read() {
        let (service, notifier) = service();
        let alice = user(&service).await;

        let refused = service
            .activity(
                alice,
                ConversationId::new(),
                ActivityState::Typing,
                Some("a\nb"),
            )
            .await;

        assert!(matches!(
            refused,
            Err(DomainError::Invalid(ValidationError::ActivityLabel(_)))
        ));
        assert_eq!(notifier.take(), vec![]);
    }

    #[test]
    fn labels_follow_the_value_rules() {
        for refused in [
            "a".repeat(MAX_ACTIVITY_LABEL_CHARS + 1),
            "\u{202E}x".to_owned(),
            "a\nb".to_owned(),
            "   ".to_owned(),
            "a\u{2028}b".to_owned(),
        ] {
            assert!(
                matches!(
                    validate_label(Some(&refused)),
                    Err(ValidationError::ActivityLabel(_))
                ),
                "{refused:?}"
            );
        }
        let longest = "é".repeat(MAX_ACTIVITY_LABEL_CHARS);
        assert_eq!(validate_label(Some(&longest)), Ok(Some(longest.clone())));
        assert_eq!(
            validate_label(Some("https://x.io")),
            Ok(Some("https://x.io".to_owned()))
        );
        assert_eq!(
            validate_label(Some("<b>tests</b>")),
            Ok(Some("<b>tests</b>".to_owned()))
        );
        assert_eq!(validate_label(Some("")), Ok(None));
        assert_eq!(validate_label(None), Ok(None));
    }
}
