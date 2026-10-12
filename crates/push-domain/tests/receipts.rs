mod common;

use common::{Person, World, delete, entries_for};
use push_domain::{ConversationId, Event, MessageRef, ReceiptUpdate};

fn receipt(
    conversation: ConversationId,
    to: &Person,
    from: &Person,
    delivered_seq: i64,
    read_seq: i64,
) -> Event {
    Event::Receipt {
        conversation,
        to: vec![to.user],
        receipt: ReceiptUpdate::Direct {
            user_id: from.user,
            delivered_seq,
            read_seq,
        },
    }
}

fn read_moved(person: &Person, conversation: ConversationId, read_seq: i64) -> Event {
    Event::ReadPositionChanged {
        user: person.user,
        conversation,
        read_seq,
    }
}

#[tokio::test]
async fn an_ack_over_a_message_from_the_peer_sends_a_delivery_receipt() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.notifier.take();

    world.svc.ack(bob.device, chat, 1).await.unwrap();
    assert_eq!(world.notifier.take(), [receipt(chat, &alice, &bob, 1, 0)]);
}

#[tokio::test]
async fn an_ack_that_does_not_move_the_users_delivered_position_sends_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let tablet = world.add_device(&bob).await;
    world.send(&alice, chat, "one").await;
    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.notifier.take();

    assert_eq!(world.svc.ack(tablet, chat, 1).await, Ok(true));
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn an_ack_over_only_ones_own_messages_sends_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.notifier.take();

    world.svc.ack(alice.device, chat, 1).await.unwrap();
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn an_ack_over_only_changes_or_deleted_messages_sends_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.edit(&alice, chat, &hello, "hello!").await;
    let oops = world.send(&alice, chat, "oops").await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(oops.message_id)))
        .await
        .unwrap();
    world.notifier.take();

    assert_eq!(world.svc.ack(bob.device, chat, 4).await, Ok(true));
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn reading_a_message_from_the_peer_sends_a_read_receipt_once() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.svc.ack(bob.device, chat, 1).await.unwrap();
    world.notifier.take();

    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [read_moved(&bob, chat, 1), receipt(chat, &alice, &bob, 1, 1)]
    );

    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    assert!(world.notifier.take().is_empty());
}

#[tokio::test]
async fn reading_past_an_edit_tells_only_the_readers_devices() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.svc.read_up_to(bob.user, chat, 1).await.unwrap();
    world.edit(&alice, chat, &hello, "hello!").await;
    world.notifier.take();

    assert_eq!(world.svc.read_up_to(bob.user, chat, 2).await, Ok(true));
    assert_eq!(world.notifier.take(), [read_moved(&bob, chat, 2)]);
}

#[tokio::test]
async fn replying_reports_that_the_peers_received_messages_were_read_and_so_delivered() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "hi").await;
    world.svc.ack(alice.device, chat, 1).await.unwrap();
    world.notifier.take();

    world.send(&alice, chat, "hello").await;
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&[&alice, &bob]),
            read_moved(&alice, chat, 1),
            receipt(chat, &bob, &alice, 1, 1)
        ]
    );
}

#[tokio::test]
async fn replying_without_having_received_the_peers_message_sends_no_read_receipt() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&bob, chat, "hi").await;
    world.notifier.take();

    world.send(&alice, chat, "hello").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);
}

#[tokio::test]
async fn reading_never_reports_messages_below_the_readers_floor() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.set_visibility(chat, bob.user, 3, None).await;
    world.send(&bob, chat, "three").await;
    world.notifier.take();

    world.svc.read_up_to(bob.user, chat, 3).await.unwrap();
    assert_eq!(world.notifier.take(), [read_moved(&bob, chat, 3)]);
}

#[tokio::test]
async fn replying_never_reports_messages_below_the_senders_floor() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.set_visibility(chat, bob.user, 3, None).await;
    world.send(&bob, chat, "three").await;
    world.svc.ack(bob.device, chat, 3).await.unwrap();
    world.notifier.take();

    world.send(&bob, chat, "four").await;
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob]), read_moved(&bob, chat, 3)]
    );
}
