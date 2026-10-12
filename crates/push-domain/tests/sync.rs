mod common;

use common::{World, bodies, seqs};
use push_domain::DomainError;

#[tokio::test]
async fn the_recipient_receives_new_messages_in_order() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["one", "two"]);
    assert_eq!(seqs(&batch.entries), [1, 2]);
    assert!(!batch.more);
}

#[tokio::test]
async fn the_senders_own_devices_also_receive_the_message() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    let batch = world.svc.sync(alice.device, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["one"]);
}

#[tokio::test]
async fn an_ack_is_cumulative_and_stops_redelivery() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }

    assert_eq!(world.svc.ack(bob.device, chat, 2).await, Ok(true));
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["three"]);
}

#[tokio::test]
async fn an_ack_never_moves_backwards() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;

    assert_eq!(world.svc.ack(bob.device, chat, 2).await, Ok(true));
    assert_eq!(world.svc.ack(bob.device, chat, 1).await, Ok(false));
    assert!(
        world
            .svc
            .sync(bob.device, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn an_ack_beyond_the_last_message_is_rejected() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.ack(bob.device, chat, 5).await,
        Err(DomainError::SeqOutOfRange)
    );
}

#[tokio::test]
async fn a_new_device_starts_up_to_date() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let tablet = world.add_device(&bob).await;
    assert!(
        world
            .svc
            .sync(tablet, 100)
            .await
            .unwrap()
            .entries
            .is_empty()
    );

    world.send(&alice, chat, "three").await;
    let batch = world.svc.sync(tablet, 100).await.unwrap();
    assert_eq!(bodies(&batch.entries), ["three"]);
}

#[tokio::test]
async fn sync_respects_the_batch_limit() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["1", "2", "3", "4", "5"] {
        world.send(&alice, chat, body).await;
    }

    let first = world.svc.sync(bob.device, 2).await.unwrap();
    assert_eq!(seqs(&first.entries), [1, 2]);
    assert!(first.more);

    world.svc.ack(bob.device, chat, 2).await.unwrap();
    let rest = world.svc.sync(bob.device, 10).await.unwrap();
    assert_eq!(seqs(&rest.entries), [3, 4, 5]);
    assert!(!rest.more);
}

#[tokio::test]
async fn sync_covers_every_conversation_of_the_user() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let with_alice = world.direct_chat(&alice, &bob).await;
    let with_carol = world.direct_chat(&carol, &bob).await;
    world.send(&alice, with_alice, "from alice").await;
    world.send(&carol, with_carol, "from carol").await;

    let mut received = bodies(&world.svc.sync(bob.device, 100).await.unwrap().entries);
    received.sort();
    assert_eq!(received, ["from alice", "from carol"]);
}

#[tokio::test]
async fn a_revoked_device_can_neither_sync_nor_ack() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.svc.revoke_device(bob.device).await.unwrap();

    assert_eq!(
        world.svc.sync(bob.device, 100).await,
        Err(DomainError::UnknownDevice)
    );
    assert_eq!(
        world.svc.ack(bob.device, chat, 1).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn a_full_batch_reports_more_only_if_something_remains() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["1", "2", "3", "4", "5"] {
        world.send(&alice, chat, body).await;
    }
    let exact = world.svc.sync(bob.device, 5).await.unwrap();
    assert_eq!(exact.entries.len(), 5);
    assert!(!exact.more);

    let other = world.direct_chat(&carol, &bob).await;
    world.send(&carol, other, "extra").await;
    let full = world.svc.sync(bob.device, 5).await.unwrap();
    assert_eq!(full.entries.len(), 5);
    assert!(full.more);
}

#[tokio::test]
async fn a_zero_limit_is_treated_as_one() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;

    let batch = world.svc.sync(bob.device, 0).await.unwrap();
    assert_eq!(seqs(&batch.entries), [1]);
    assert!(batch.more);
}

#[tokio::test]
async fn a_non_member_cannot_ack() {
    let world = World::new();
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.ack(carol.device, chat, 1).await,
        Err(DomainError::NotMember)
    );
}

#[tokio::test]
async fn a_negative_ack_is_rejected() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    assert_eq!(
        world.svc.ack(bob.device, chat, -1).await,
        Err(DomainError::SeqOutOfRange)
    );
}

#[tokio::test]
async fn sync_without_ack_returns_the_same_entries() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;

    let first = world.svc.sync(bob.device, 100).await.unwrap();
    let second = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(seqs(&second.entries), [1]);
}

#[tokio::test]
async fn a_new_device_gets_nothing_from_a_conversation_already_left() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    for body in ["one", "two", "three"] {
        world.send(&alice, chat, body).await;
    }
    world.set_visibility(chat, bob.user, 1, Some(2)).await;

    let new_device = world.add_device(&bob).await;
    assert_eq!(world.position(new_device, chat).await, Some(2));
    let batch = world.svc.sync(new_device, 100).await.unwrap();
    assert!(batch.entries.is_empty());
    let history = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [1, 2]);
}
