mod common;

use chrono::TimeDelta;
use common::{Person, World, bodies, edit, entries_for, seqs, text};
use push_domain::{
    ClientMsgId, Clock, ConversationId, DomainError, EditText, Entry, EntryKind, MessageId,
    MessageRef, Policy, SendReceipt, SendText, ValidationError,
};

/// Alice and bob in a direct chat, with one message "hello" from alice at seq 1.
async fn chat_with_one_message() -> (World, Person, Person, ConversationId, SendReceipt) {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    (world, alice, bob, chat, hello)
}

fn by_id(target: &SendReceipt) -> MessageRef {
    MessageRef::Id(target.message_id)
}

async fn synced(world: &World, person: &Person) -> Vec<Entry> {
    world.svc.sync(person.device, 100).await.unwrap().entries
}

#[tokio::test]
async fn an_edit_is_an_entry_of_its_own_and_updates_the_message() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.clock.advance(TimeDelta::minutes(1));

    let receipt = world.edit(&alice, chat, &hello, "hello again").await;
    assert_eq!(receipt.seq, 2);

    let entries = synced(&world, &bob).await;
    assert_eq!(seqs(&entries), [1, 2]);
    assert_eq!(bodies(&entries), ["hello again", "hello again"]);
    let (message, change) = (&entries[0], &entries[1]);
    assert_eq!(message.kind, EntryKind::Message);
    assert_eq!(message.edited_at, Some(world.clock.now()));
    assert_eq!(message.edit_seq, Some(2));
    assert_eq!(change.kind, EntryKind::Edit);
    assert_eq!(change.target_seq, Some(1));
    assert_eq!(change.message_id, receipt.message_id);
}

#[tokio::test]
async fn only_the_latest_edit_keeps_its_text() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.edit(&alice, chat, &hello, "v1").await;
    world.edit(&alice, chat, &hello, "v2").await;

    let entries = synced(&world, &bob).await;
    let texts: Vec<Option<&[u8]>> = entries.iter().map(|entry| entry.body.as_deref()).collect();
    assert_eq!(
        texts,
        [Some(&b"v2"[..]), None, Some(&b"v2"[..])],
        "message, first edit, second edit"
    );
    assert_eq!(entries[0].edit_seq, Some(3));
}

#[tokio::test]
async fn a_message_can_be_targeted_by_its_client_msg_id() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let command = text(&alice, chat, "hello");
    let target = MessageRef::ClientId(command.client_msg_id);
    world.svc.send_text(command).await.unwrap();

    world
        .svc
        .edit_text(edit(&alice, chat, target, "fixed"))
        .await
        .unwrap();
    assert_eq!(bodies(&synced(&world, &bob).await)[0], "fixed");
}

#[tokio::test]
async fn an_edit_retry_returns_the_original_and_changes_nothing() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let command = edit(&alice, chat, by_id(&hello), "v1");
    let first = world.svc.edit_text(command.clone()).await.unwrap();
    world.notifier.take();

    let retry = world.svc.edit_text(command).await.unwrap();
    assert_eq!(retry, first);
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.send(&alice, chat, "next").await.seq, 3);
}

#[tokio::test]
async fn an_edit_retry_succeeds_even_after_the_window_has_passed() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let command = edit(&alice, chat, by_id(&hello), "v1");
    let first = world.svc.edit_text(command.clone()).await.unwrap();
    world.clock.advance(TimeDelta::days(3));

    assert_eq!(world.svc.edit_text(command).await, Ok(first));
}

#[tokio::test]
async fn only_the_sender_can_edit() {
    let (world, _alice, bob, chat, hello) = chat_with_one_message().await;
    assert_eq!(
        world
            .svc
            .edit_text(edit(&bob, chat, by_id(&hello), "mine now"))
            .await,
        Err(DomainError::NotSender)
    );
    assert_eq!(world.send(&bob, chat, "gapless").await.seq, 2);
}

#[tokio::test]
async fn the_sender_may_edit_from_another_device() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    let laptop = world.add_device(&alice).await;
    let command = EditText {
        device: laptop,
        ..edit(&alice, chat, by_id(&hello), "from the laptop")
    };

    world.svc.edit_text(command).await.unwrap();
    assert_eq!(bodies(&synced(&world, &bob).await)[0], "from the laptop");
}

#[tokio::test]
async fn the_target_must_be_a_message_of_this_conversation() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let carol = world.person().await;
    let other_chat = world.direct_chat(&alice, &carol).await;
    let elsewhere = world.send(&alice, other_chat, "elsewhere").await;
    let change = world.edit(&alice, chat, &hello, "v1").await;

    for target in [
        MessageRef::Id(MessageId::new()),
        MessageRef::ClientId(ClientMsgId::new()),
        by_id(&elsewhere),
        by_id(&change),
    ] {
        assert_eq!(
            world.svc.edit_text(edit(&alice, chat, target, "x")).await,
            Err(DomainError::TargetNotFound)
        );
    }
}

#[tokio::test]
async fn a_target_outside_the_callers_visible_range_is_not_found() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world.set_visibility(chat, alice.user, 2, None).await;

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "x"))
            .await,
        Err(DomainError::TargetNotFound)
    );
}

#[tokio::test]
async fn an_edit_is_allowed_until_the_window_closes() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world.clock.advance(TimeDelta::hours(48));
    world.edit(&alice, chat, &hello, "just in time").await;

    world.clock.advance(TimeDelta::seconds(1));
    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "too late"))
            .await,
        Err(DomainError::WindowExpired)
    );
}

#[tokio::test]
async fn the_edit_window_comes_from_the_policy() {
    let world = World::with_policy(Policy {
        edit_window: TimeDelta::minutes(1),
        ..Policy::default()
    });
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    world.clock.advance(TimeDelta::minutes(2));

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "late"))
            .await,
        Err(DomainError::WindowExpired)
    );
}

#[tokio::test]
async fn an_invalid_edit_is_rejected_and_uses_no_seq() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), ""))
            .await,
        Err(DomainError::Invalid(ValidationError::EmptyBody))
    );
    assert_eq!(world.send(&alice, chat, "next").await.seq, 2);
}

#[tokio::test]
async fn a_blocked_sender_cannot_edit() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "x"))
            .await,
        Err(DomainError::NotContact)
    );
}

#[tokio::test]
async fn a_client_msg_id_cannot_be_reused_for_another_kind_of_entry() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let command = text(&alice, chat, "hello");
    let reused = command.client_msg_id;
    let hello = world.svc.send_text(command).await.unwrap();

    let clash = EditText {
        client_msg_id: reused,
        ..edit(&alice, chat, by_id(&hello), "x")
    };
    assert_eq!(
        world.svc.edit_text(clash).await,
        Err(DomainError::ClientIdConflict)
    );
}

#[tokio::test]
async fn an_edit_tells_every_member_to_sync() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.notifier.take();

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);
}

#[tokio::test]
async fn an_edit_moves_the_conversations_updated_at() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    world.clock.advance(TimeDelta::minutes(3));

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(world.conversation(chat).await.updated_at, world.clock.now());
}

#[tokio::test]
async fn an_edit_leaves_the_senders_read_position_unchanged() {
    let (world, alice, bob, chat, hello) = chat_with_one_message().await;
    world.send(&bob, chat, "reply").await;
    let read_seq_of_alice = || async {
        let state = world
            .svc
            .conversation_state(alice.user, chat)
            .await
            .unwrap();
        state
            .members
            .iter()
            .find(|member| member.user_id == alice.user)
            .unwrap()
            .read_seq
    };
    let before = read_seq_of_alice().await;

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(read_seq_of_alice().await, before);
}

#[tokio::test]
async fn a_text_send_cannot_reuse_the_client_msg_id_of_an_edit() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    let command = edit(&alice, chat, by_id(&hello), "v1");
    let reused = command.client_msg_id;
    world.svc.edit_text(command).await.unwrap();

    let clash = SendText {
        client_msg_id: reused,
        ..text(&alice, chat, "again")
    };
    assert_eq!(
        world.svc.send_text(clash).await,
        Err(DomainError::ClientIdConflict)
    );
}

#[tokio::test]
async fn refused_edits_leave_no_gap_in_seq() {
    let (world, alice, _bob, chat, hello) = chat_with_one_message().await;
    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, MessageRef::Id(MessageId::new()), "x"))
            .await,
        Err(DomainError::TargetNotFound)
    );
    world.clock.advance(TimeDelta::hours(49));
    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "x"))
            .await,
        Err(DomainError::WindowExpired)
    );
    assert_eq!(world.send(&alice, chat, "next").await.seq, 2);
}
