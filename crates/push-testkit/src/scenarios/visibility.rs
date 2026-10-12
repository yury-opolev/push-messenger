use crate::{Backend, Case, Person, World, bodies, case_list, delete, seqs};
use push_domain::{ConversationId, DomainError, MessageRef};

/// A chat between alice and bob with six messages from alice. Bob's visibility is set
/// before the messages are sent, because a delivery position cannot be lowered.
async fn chat_with_six_messages<B: Backend>(
    backend: &B,
    visible_from_seq: i64,
    left_at_seq: Option<i64>,
) -> (World<B::Store>, Person, ConversationId) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    world
        .set_visibility(chat, bob.user, visible_from_seq, left_at_seq)
        .await;
    for _ in 0..6 {
        world.send(&alice, chat, "x").await;
    }
    (world, bob, chat)
}

async fn sync_starts_at_visible_from<B: Backend>(backend: &B) {
    let (world, bob, _chat) = chat_with_six_messages(backend, 3, None).await;
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3, 4, 5, 6]);
}

async fn sync_and_history_stop_at_left_at<B: Backend>(backend: &B) {
    let (world, bob, chat) = chat_with_six_messages(backend, 1, Some(4)).await;
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [1, 2, 3, 4]);
    let history = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [1, 2, 3, 4]);
}

async fn history_does_not_go_below_visible_from<B: Backend>(backend: &B) {
    let (world, bob, chat) = chat_with_six_messages(backend, 3, None).await;
    let history = world.svc.history(bob.user, chat, None, 10).await.unwrap();
    assert_eq!(seqs(&history.entries), [3, 4, 5, 6]);
    assert!(!history.has_more);
}

async fn ack_and_read_beyond_left_at_are_rejected<B: Backend>(backend: &B) {
    let (world, bob, chat) = chat_with_six_messages(backend, 1, Some(4)).await;
    assert_eq!(
        world.svc.ack(bob.device, chat, 5).await,
        Err(DomainError::SeqOutOfRange)
    );
    assert_eq!(
        world.svc.read_up_to(bob.user, chat, 5).await,
        Err(DomainError::SeqOutOfRange)
    );
    assert_eq!(world.svc.ack(bob.device, chat, 4).await, Ok(true));
}

async fn an_ack_below_the_floor_changes_nothing<B: Backend>(backend: &B) {
    let (world, bob, chat) = chat_with_six_messages(backend, 3, None).await;
    let before = world.position(bob.device, chat).await;

    assert_eq!(world.svc.ack(bob.device, chat, 1).await, Ok(false));
    assert_eq!(world.position(bob.device, chat).await, before);
}

async fn changes_to_messages_below_the_visible_range_are_not_delivered<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    let third = world.send(&alice, chat, "three").await;
    world.set_visibility(chat, bob.user, 3, None).await;

    world.edit(&alice, chat, &first, "one, edited").await;
    world.edit(&alice, chat, &third, "three, edited").await;
    let batch = world.svc.sync(bob.device, 100).await.unwrap();
    assert_eq!(seqs(&batch.entries), [3, 5]);
    assert_eq!(bodies(&batch.entries), ["three, edited", "three, edited"]);
    assert!(!batch.more);
}

async fn a_hidden_change_at_the_end_does_not_report_more<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let first = world.send(&alice, chat, "one").await;
    world.send(&alice, chat, "two").await;
    world.set_visibility(chat, bob.user, 2, None).await;
    world
        .svc
        .delete_message(delete(&alice, chat, MessageRef::Id(first.message_id)))
        .await
        .unwrap();

    let batch = world.svc.sync(bob.device, 1).await.unwrap();
    assert_eq!(seqs(&batch.entries), [2]);
    assert!(!batch.more);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        sync_starts_at_visible_from,
        sync_and_history_stop_at_left_at,
        history_does_not_go_below_visible_from,
        ack_and_read_beyond_left_at_are_rejected,
        an_ack_below_the_floor_changes_nothing,
        changes_to_messages_below_the_visible_range_are_not_delivered,
        a_hidden_change_at_the_end_does_not_report_more,
    ]
}
