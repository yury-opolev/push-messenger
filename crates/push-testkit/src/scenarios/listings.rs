use crate::{Backend, Case, Person, World, case_list, delete};
use chrono::TimeDelta;
use push_domain::{
    Clock, ConversationId, ConversationKind, ConversationSummary, MAX_UNREAD, MessageRef, Receipts,
    Store, UserId,
};

fn peer(peer_delivered_seq: i64, peer_read_seq: i64) -> Receipts {
    Receipts::Direct {
        peer_delivered_seq,
        peer_read_seq,
    }
}

async fn summary<S: Store>(
    world: &World<S>,
    person: &Person,
    chat: ConversationId,
) -> ConversationSummary {
    world
        .svc
        .list_conversations(person.user, None)
        .await
        .unwrap()
        .into_iter()
        .find(|summary| summary.conversation_id == chat)
        .expect("conversation listed")
}

async fn listed<S: Store>(world: &World<S>, person: &Person) -> Vec<ConversationId> {
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

async fn a_new_direct_conversation_is_listed_for_both_members<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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
                receipts: peer(0, 0),
                updated_at: world.clock.now(),
                members: sorted(vec![alice.user, bob.user]),
                admins: vec![],
            }]
        );
    }
}

async fn unread_counts_messages_from_others_after_the_read_position<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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
    assert_eq!((alices.read_seq, alices.unread), (0, 0));
}

async fn unread_skips_changes_and_deleted_messages<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn unread_is_capped<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for _ in 0..=MAX_UNREAD {
        world.send(&alice, chat, "x").await;
    }

    assert_eq!(summary(&world, &bob, chat).await.unread, MAX_UNREAD);
}

async fn unread_and_last_seq_stay_within_the_visible_range<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn changed_since_lists_only_conversations_updated_later<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn a_blocked_conversation_is_hidden_from_the_blocker_only<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn peer_positions_are_zero_until_the_peer_acks_or_reads<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(summary(&world, &alice, chat).await.receipts, peer(0, 0));
}

async fn peer_positions_are_listed_but_do_not_count_as_changes<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }
    let since = world.clock.now();
    world.clock.advance(TimeDelta::minutes(1));

    world.svc.ack(bob.device, chat, 2).await.unwrap();
    assert_eq!(summary(&world, &alice, chat).await.receipts, peer(2, 0));

    world.svc.read_up_to(bob.user, chat, 3).await.unwrap();
    assert_eq!(
        summary(&world, &alice, chat).await.receipts,
        peer(3, 3),
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

async fn a_group_is_listed_with_its_admins_and_group_wide_positions<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.send(&alice, group, "one").await;
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();

    let alices = summary(&world, &alice, group).await;
    assert_eq!(
        alices,
        ConversationSummary {
            conversation_id: group,
            kind: ConversationKind::Group,
            title: Some("Group".into()),
            last_seq: 2,
            read_seq: 0,
            unread: 0,
            receipts: Receipts::Group {
                all_delivered_seq: 1,
                all_read_seq: 1,
            },
            updated_at: world.clock.now(),
            members: sorted(vec![alice.user, bob.user, carol.user]),
            admins: vec![alice.user],
        }
    );

    world.svc.read_up_to(carol.user, group, 2).await.unwrap();
    assert_eq!(
        summary(&world, &bob, group).await.receipts,
        Receipts::Group {
            all_delivered_seq: 2,
            all_read_seq: 2,
        }
    );
}

async fn unread_in_a_group_counts_messages_from_everyone_else<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.send(&bob, group, "from bob").await;
    world.send(&carol, group, "from carol").await;

    assert_eq!(summary(&world, &alice, group).await.unread, 2);
    assert_eq!(summary(&world, &bob, group).await.unread, 1);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_new_direct_conversation_is_listed_for_both_members,
        unread_counts_messages_from_others_after_the_read_position,
        unread_skips_changes_and_deleted_messages,
        unread_is_capped,
        unread_and_last_seq_stay_within_the_visible_range,
        changed_since_lists_only_conversations_updated_later,
        a_blocked_conversation_is_hidden_from_the_blocker_only,
        peer_positions_are_zero_until_the_peer_acks_or_reads,
        peer_positions_are_listed_but_do_not_count_as_changes,
        a_group_is_listed_with_its_admins_and_group_wide_positions,
        unread_in_a_group_counts_messages_from_everyone_else,
    ]
}
