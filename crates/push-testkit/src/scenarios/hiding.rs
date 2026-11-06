use chrono::TimeDelta;
use push_domain::{
    Clock, ConversationId, ConversationSummary, DomainError, Event, Store, ValidationError,
};

use crate::{Backend, Case, Person, World, case_list};

async fn listing<S: Store>(
    world: &World<S>,
    person: &Person,
    include_hidden: bool,
) -> Vec<ConversationSummary> {
    world
        .svc
        .list_conversations_with(person.user, None, include_hidden)
        .await
        .unwrap()
}

/// `(conversation, hidden, peer_deleted)` of each listed conversation.
async fn flags<S: Store>(
    world: &World<S>,
    person: &Person,
    include_hidden: bool,
) -> Vec<(ConversationId, bool, bool)> {
    listing(world, person, include_hidden)
        .await
        .iter()
        .map(|summary| {
            (
                summary.conversation_id,
                summary.hidden,
                summary.peer_deleted,
            )
        })
        .collect()
}

async fn changed_since<S: Store>(
    world: &World<S>,
    person: &Person,
    since: chrono::DateTime<chrono::Utc>,
) -> Vec<(ConversationId, bool)> {
    world
        .svc
        .list_conversations_with(person.user, Some(since), true)
        .await
        .unwrap()
        .iter()
        .map(|summary| (summary.conversation_id, summary.hidden))
        .collect()
}

fn flags_event(person: &Person, chat: ConversationId, hidden: bool, peer_deleted: bool) -> Event {
    Event::ConversationFlags {
        user: person.user,
        conversation: chat,
        hidden,
        peer_deleted,
    }
}

async fn hiding_leaves_it_out_of_the_users_own_listing_only<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.notifier.take();

    world.svc.hide_conversation(alice.user, chat).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [flags_event(&alice, chat, true, false)]
    );
    assert!(flags(&world, &alice, false).await.is_empty());
    assert_eq!(flags(&world, &alice, true).await, [(chat, true, false)]);
    assert_eq!(flags(&world, &bob, false).await, [(chat, false, false)]);

    // Idempotent: nothing changes, nothing is published.
    world.svc.hide_conversation(alice.user, chat).await.unwrap();
    assert_eq!(world.notifier.take(), []);

    world
        .svc
        .unhide_conversation(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [flags_event(&alice, chat, false, false)]
    );
    assert_eq!(flags(&world, &alice, false).await, [(chat, false, false)]);
    world
        .svc
        .unhide_conversation(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), []);
}

async fn only_a_member_hides_and_only_a_direct_conversation<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    let dave = world.person().await;
    let group = world.group(&alice, &[&dave]).await;

    assert_eq!(
        world.svc.hide_conversation(carol.user, chat).await,
        Err(DomainError::NotMember)
    );
    assert_eq!(
        world.svc.hide_conversation(alice.user, group).await,
        Err(DomainError::Invalid(ValidationError::NotDirect))
    );
    assert_eq!(
        world
            .svc
            .unhide_conversation(alice.user, ConversationId::new())
            .await,
        Err(DomainError::ConversationNotFound)
    );
}

async fn a_new_message_shows_a_hidden_conversation_again<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.svc.hide_conversation(alice.user, chat).await.unwrap();
    world.notifier.take();

    world.send(&bob, chat, "still there?").await;
    let events = world.notifier.take();
    assert!(
        events.contains(&flags_event(&alice, chat, false, false)),
        "{events:?}"
    );
    assert_eq!(flags(&world, &alice, false).await, [(chat, false, false)]);

    // A message with nobody hiding it publishes no flags.
    world.send(&bob, chat, "again").await;
    assert!(
        !world
            .notifier
            .take()
            .iter()
            .any(|event| matches!(event, Event::ConversationFlags { .. }))
    );
}

async fn a_flag_change_counts_for_changed_since<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    world.direct_chat(&alice, &carol).await;
    world.clock.advance(TimeDelta::minutes(10));
    let since = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));
    assert!(changed_since(&world, &alice, since).await.is_empty());

    world.svc.hide_conversation(alice.user, chat).await.unwrap();
    // The other conversation, unchanged, is not listed.
    assert_eq!(changed_since(&world, &alice, since).await, [(chat, true)]);
    // Hidden ones stay out of a listing without include_hidden, changed or not.
    assert!(
        world
            .svc
            .list_conversations_with(alice.user, Some(since), false)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn a_deleted_peer_is_flagged_and_its_peers_are_told<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let agent = world.person().await;
    let with_bob = world.direct_chat(&alice, &bob).await;
    let with_agent = world.direct_chat(&agent, &alice).await;
    let carol_agent = world.direct_chat(&carol, &agent).await;
    world
        .svc
        .hide_conversation(carol.user, carol_agent)
        .await
        .unwrap();
    world.clock.advance(TimeDelta::minutes(1));
    let since = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));
    world.notifier.take();

    world.svc.user_deleted(agent.user).await.unwrap();
    let mut events = world.notifier.take();
    events.sort_by_key(|event| format!("{event:?}"));
    let mut expected = vec![
        flags_event(&alice, with_agent, false, true),
        flags_event(&carol, carol_agent, true, true),
    ];
    expected.sort_by_key(|event| format!("{event:?}"));
    assert_eq!(events, expected);

    let mut alices = flags(&world, &alice, false).await;
    alices.sort();
    let mut expected = vec![(with_bob, false, false), (with_agent, false, true)];
    expected.sort();
    assert_eq!(alices, expected);
    assert_eq!(
        flags(&world, &carol, true).await,
        [(carol_agent, true, true)]
    );
    // The deletion counts as a change since before it.
    assert_eq!(
        world
            .svc
            .list_conversations_with(alice.user, Some(since), false)
            .await
            .unwrap()
            .iter()
            .map(|summary| summary.conversation_id)
            .collect::<Vec<_>>(),
        [with_agent]
    );

    // A repeated relay changes and publishes nothing.
    world.svc.user_deleted(agent.user).await.unwrap();
    assert_eq!(world.notifier.take(), []);
}

async fn a_deletion_of_an_unknown_user_is_recorded<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let stranger = push_domain::UserId::new();
    world.svc.user_deleted(stranger).await.unwrap();
    world.svc.user_deleted(stranger).await.unwrap();
    assert_eq!(world.notifier.take(), []);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        hiding_leaves_it_out_of_the_users_own_listing_only,
        only_a_member_hides_and_only_a_direct_conversation,
        a_new_message_shows_a_hidden_conversation_again,
        a_flag_change_counts_for_changed_since,
        a_deleted_peer_is_flagged_and_its_peers_are_told,
        a_deletion_of_an_unknown_user_is_recorded,
    ]
}
