mod common;

use chrono::TimeDelta;
use common::{Person, World, delete};
use push_domain::{
    Clock, ConversationId, ConversationKind, ConversationSummary, MAX_UNREAD, MessageRef, UserId,
};

async fn summary(world: &World, person: &Person, chat: ConversationId) -> ConversationSummary {
    world
        .svc
        .list_conversations(person.user, None)
        .await
        .unwrap()
        .into_iter()
        .find(|summary| summary.conversation_id == chat)
        .expect("conversation listed")
}

async fn listed(world: &World, person: &Person) -> Vec<ConversationId> {
    world
        .svc
        .list_conversations(person.user, None)
        .await
        .unwrap()
        .iter()
        .map(|summary| summary.conversation_id)
        .collect()
}

fn sorted(mut users: Vec<UserId>) -> Vec<UserId> {
    users.sort();
    users
}

#[tokio::test]
async fn a_new_direct_conversation_is_listed_for_both_members() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    for person in [&alice, &bob] {
        let listed = world
            .svc
            .list_conversations(person.user, None)
            .await
            .unwrap();
        assert_eq!(
            listed,
            [ConversationSummary {
                conversation_id: chat,
                kind: ConversationKind::Direct,
                title: None,
                last_seq: 0,
                read_seq: 0,
                unread: 0,
                peer_delivered_seq: 0,
                peer_read_seq: 0,
                updated_at: world.clock.now(),
                members: sorted(vec![alice.user, bob.user]),
            }]
        );
    }
}

#[tokio::test]
async fn unread_counts_messages_from_others_after_the_read_position() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.read_seq, bobs.unread), (3, 0, 3));
    world.svc.read_up_to(bob.user, chat, 2).await.unwrap();
    assert_eq!(summary(&world, &bob, chat).await.unread, 1);

    let alices = summary(&world, &alice, chat).await;
    assert_eq!((alices.read_seq, alices.unread), (3, 0));
}

#[tokio::test]
async fn unread_skips_changes_and_deleted_messages() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let kept = world.send(&alice, chat, "kept").await;
    let gone = world.send(&alice, chat, "gone").await;
    world.edit(&alice, chat, &kept, "kept, edited").await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(gone.message_id)))
        .await
        .unwrap();

    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.unread), (4, 1));
}

#[tokio::test]
async fn unread_is_capped() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for _ in 0..=MAX_UNREAD {
        world.send(&alice, chat, "x").await;
    }

    assert_eq!(summary(&world, &bob, chat).await.unread, MAX_UNREAD);
}

#[tokio::test]
async fn unread_and_last_seq_stay_within_the_visible_range() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for _ in 0..5 {
        world.send(&alice, chat, "x").await;
    }

    world.set_visibility(chat, bob.user, 3, None).await;
    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.unread), (5, 3));

    world.set_visibility(chat, bob.user, 1, Some(2)).await;
    let bobs = summary(&world, &bob, chat).await;
    assert_eq!((bobs.last_seq, bobs.unread), (2, 2));
}

#[tokio::test]
async fn changed_since_lists_only_conversations_updated_later() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let quiet = world.direct_chat(&alice, &bob).await;
    let busy = world.direct_chat(&alice, &carol).await;
    let since = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));
    world.send(&carol, busy, "news").await;

    let changed = world
        .svc
        .list_conversations(alice.user, Some(since))
        .await
        .unwrap();
    assert_eq!(
        changed
            .iter()
            .map(|summary| summary.conversation_id)
            .collect::<Vec<_>>(),
        [busy]
    );
    assert!(
        world
            .svc
            .list_conversations(alice.user, Some(world.clock.now()))
            .await
            .unwrap()
            .is_empty()
    );
    let mut all = vec![quiet, busy];
    all.sort();
    assert_eq!(listed(&world, &alice).await, all);
}

#[tokio::test]
async fn a_blocked_conversation_is_hidden_from_the_blocker_only() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;

    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert!(listed(&world, &alice).await.is_empty());
    assert_eq!(listed(&world, &bob).await, [chat]);

    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(listed(&world, &alice).await, [chat]);
}

#[tokio::test]
async fn peer_positions_are_zero_until_the_peer_acks_or_reads() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    let alices = summary(&world, &alice, chat).await;
    assert_eq!((alices.peer_delivered_seq, alices.peer_read_seq), (0, 0));
}

#[tokio::test]
async fn peer_positions_are_listed_but_do_not_count_as_changes() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }
    let since = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));

    world.svc.ack(bob.device, chat, 2).await.unwrap();
    let alices = summary(&world, &alice, chat).await;
    assert_eq!((alices.peer_delivered_seq, alices.peer_read_seq), (2, 0));

    world.svc.read_up_to(bob.user, chat, 3).await.unwrap();
    let alices = summary(&world, &alice, chat).await;
    assert_eq!(
        (alices.peer_delivered_seq, alices.peer_read_seq),
        (3, 3),
        "read implies delivered"
    );
    assert!(
        world
            .svc
            .list_conversations(alice.user, Some(since))
            .await
            .unwrap()
            .is_empty(),
        "an ack or a read is not a content change"
    );
}
