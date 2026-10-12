use crate::{Backend, Case, Person, World, case_list, delete, entries_for};
use push_domain::{ConversationId, Event, MessageRef, ReceiptUpdate};

fn group_receipt(
    conversation: ConversationId,
    to: &[&Person],
    all_delivered_seq: i64,
    all_read_seq: i64,
) -> Event {
    let mut users: Vec<_> = to.iter().map(|person| person.user).collect();
    users.sort();
    Event::Receipt {
        conversation,
        to: users,
        receipt: ReceiptUpdate::Group {
            all_delivered_seq,
            all_read_seq,
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

/// A group of alice (admin), bob and carol; `seq` 1 is its `Created` entry.
async fn trio<B: Backend>(
    backend: &B,
) -> (World<B::Store>, Person, Person, Person, ConversationId) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let group = world.group(&alice, &[&bob, &carol]).await;
    world.notifier.take();
    (world, alice, bob, carol, group)
}

async fn a_group_receipt_waits_for_every_other_member<B: Backend>(backend: &B) {
    let (world, alice, bob, carol, group) = trio(backend).await;
    world.send(&alice, group, "hi").await;
    world.notifier.take();
    let everyone = [&alice, &bob, &carol];

    world.svc.ack(bob.device, group, 2).await.unwrap();
    assert!(world.notifier.take().is_empty());
    world.svc.ack(carol.device, group, 2).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [group_receipt(group, &everyone, 2, 1)]
    );

    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    assert_eq!(world.notifier.take(), [read_moved(&bob, group, 2)]);
    world.svc.read_up_to(carol.user, group, 2).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            read_moved(&carol, group, 2),
            group_receipt(group, &everyone, 2, 2)
        ]
    );
}

async fn ones_own_messages_never_hold_the_group_back<B: Backend>(backend: &B) {
    let (world, alice, bob, carol, group) = trio(backend).await;
    world.send(&alice, group, "one").await;
    world.send(&bob, group, "two").await;
    world.svc.read_up_to(carol.user, group, 3).await.unwrap();
    world.svc.read_up_to(alice.user, group, 3).await.unwrap();
    world.notifier.take();

    world.svc.read_up_to(bob.user, group, 3).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            read_moved(&bob, group, 3),
            group_receipt(group, &[&alice, &bob, &carol], 3, 3)
        ]
    );
}

async fn replying_in_a_group_can_complete_a_read<B: Backend>(backend: &B) {
    let (world, alice, bob, carol, group) = trio(backend).await;
    world.send(&alice, group, "one").await;
    world.svc.ack(bob.device, group, 2).await.unwrap();
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    world.svc.ack(carol.device, group, 2).await.unwrap();
    world.notifier.take();

    world.send(&carol, group, "reply").await;
    let everyone = [&alice, &bob, &carol];
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&everyone),
            read_moved(&carol, group, 2),
            group_receipt(group, &everyone, 2, 2)
        ]
    );
}

async fn edits_send_no_group_receipt<B: Backend>(backend: &B) {
    let (world, alice, bob, carol, group) = trio(backend).await;
    let hello = world.send(&alice, group, "hello").await;
    world.svc.read_up_to(bob.user, group, 2).await.unwrap();
    world.svc.read_up_to(carol.user, group, 2).await.unwrap();
    world.notifier.take();

    world.edit(&alice, group, &hello, "hello!").await;
    assert_eq!(
        world.notifier.take(),
        [entries_for(&[&alice, &bob, &carol])]
    );
}

async fn deleting_the_message_someone_had_not_read_can_complete_a_read<B: Backend>(backend: &B) {
    let (world, alice, bob, carol, group) = trio(backend).await;
    let unread = world.send(&alice, group, "one").await;
    world.send(&carol, group, "two").await;
    world.svc.read_up_to(alice.user, group, 3).await.unwrap();
    world.svc.read_up_to(bob.user, group, 3).await.unwrap();
    world.notifier.take();

    world
        .svc
        .delete_message(delete(&alice, group, MessageRef::Id(unread.message_id)))
        .await
        .unwrap();
    let everyone = [&alice, &bob, &carol];
    assert_eq!(
        world.notifier.take(),
        [
            entries_for(&everyone),
            group_receipt(group, &everyone, 4, 4)
        ]
    );
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_group_receipt_waits_for_every_other_member,
        ones_own_messages_never_hold_the_group_back,
        replying_in_a_group_can_complete_a_read,
        edits_send_no_group_receipt,
        deleting_the_message_someone_had_not_read_can_complete_a_read,
    ]
}
