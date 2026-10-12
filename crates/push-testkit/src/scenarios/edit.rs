use crate::{Backend, Case, Person, World, bodies, case_list, edit, entries_for, seqs, text};
use chrono::TimeDelta;
use push_domain::{
    ClientMsgId, Clock, ConversationId, DomainError, EditText, Entry, EntryKind, MessageId,
    MessageRef, Policy, Positions, SendReceipt, SendText, Store, ValidationError,
};

/// Alice and bob in a direct chat, with one message "hello" from alice at seq 1.
async fn chat_with_one_message<B: Backend>(
    backend: &B,
) -> (World<B::Store>, Person, Person, ConversationId, SendReceipt) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let hello = world.send(&alice, chat, "hello").await;
    (world, alice, bob, chat, hello)
}

fn by_id(target: &SendReceipt) -> MessageRef {
    MessageRef::Id(target.message_id)
}

async fn synced<S: Store>(world: &World<S>, person: &Person) -> Vec<Entry> {
    world.svc.sync(person.device, 100).await.unwrap().entries
}

async fn an_edit_is_an_entry_of_its_own_and_updates_the_message<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn only_the_latest_edit_keeps_its_text<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn a_message_can_be_targeted_by_its_client_msg_id<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn an_edit_retry_returns_the_original_and_changes_nothing<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
    let command = edit(&alice, chat, by_id(&hello), "v1");
    let first = world.svc.edit_text(command.clone()).await.unwrap();
    world.notifier.take();

    let retry = world.svc.edit_text(command).await.unwrap();
    assert_eq!(retry, first);
    assert!(world.notifier.take().is_empty());
    assert_eq!(world.send(&alice, chat, "next").await.seq, 3);
}

async fn an_edit_retry_succeeds_even_after_the_window_has_passed<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
    let command = edit(&alice, chat, by_id(&hello), "v1");
    let first = world.svc.edit_text(command.clone()).await.unwrap();
    world.clock.advance(TimeDelta::days(3));

    assert_eq!(world.svc.edit_text(command).await, Ok(first));
}

async fn only_the_sender_can_edit<B: Backend>(backend: &B) {
    let (world, _alice, bob, chat, hello) = chat_with_one_message(backend).await;
    assert_eq!(
        world
            .svc
            .edit_text(edit(&bob, chat, by_id(&hello), "mine now"))
            .await,
        Err(DomainError::NotSender)
    );
    assert_eq!(world.send(&bob, chat, "gapless").await.seq, 2);
}

async fn the_sender_may_edit_from_another_device<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
    let laptop = world.add_device(&alice).await;
    let command = EditText {
        device: laptop,
        ..edit(&alice, chat, by_id(&hello), "from the laptop")
    };

    world.svc.edit_text(command).await.unwrap();
    assert_eq!(bodies(&synced(&world, &bob).await)[0], "from the laptop");
}

async fn the_target_must_be_a_message_of_this_conversation<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn a_target_outside_the_callers_visible_range_is_not_found<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
    world.set_visibility(chat, alice.user, 2, None).await;

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "x"))
            .await,
        Err(DomainError::TargetNotFound)
    );
}

async fn an_edit_is_allowed_until_the_window_closes<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn the_edit_window_comes_from_the_policy<B: Backend>(backend: &B) {
    let world = World::with_policy(
        backend,
        Policy {
            edit_window: TimeDelta::minutes(1),
            ..Policy::default()
        },
    )
    .await;
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

async fn an_invalid_edit_is_rejected_and_uses_no_seq<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), ""))
            .await,
        Err(DomainError::Invalid(ValidationError::EmptyBody))
    );
    assert_eq!(world.send(&alice, chat, "next").await.seq, 2);
}

async fn a_blocked_sender_cannot_edit<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world
            .svc
            .edit_text(edit(&alice, chat, by_id(&hello), "x"))
            .await,
        Err(DomainError::NotContact)
    );
}

async fn a_client_msg_id_cannot_be_reused_for_another_kind_of_entry<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn an_edit_tells_every_member_to_sync<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
    world.notifier.take();

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(world.notifier.take(), [entries_for(&[&alice, &bob])]);
}

async fn an_edit_moves_the_conversations_updated_at<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
    world.clock.advance(TimeDelta::minutes(3));

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(world.conversation(chat).await.updated_at, world.clock.now());
}

async fn an_edit_leaves_the_senders_read_position_unchanged<B: Backend>(backend: &B) {
    let (world, alice, bob, chat, hello) = chat_with_one_message(backend).await;
    world.send(&bob, chat, "reply").await;
    let read_seq_of_alice = || async {
        let state = world
            .svc
            .conversation_state(alice.user, chat)
            .await
            .unwrap();
        let Positions::Members(members) = state.positions else {
            panic!("a direct conversation lists its members' positions");
        };
        members
            .iter()
            .find(|member| member.user_id == alice.user)
            .unwrap()
            .read_seq
    };
    let before = read_seq_of_alice().await;

    world.edit(&alice, chat, &hello, "v1").await;
    assert_eq!(read_seq_of_alice().await, before);
}

async fn a_text_send_cannot_reuse_the_client_msg_id_of_an_edit<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
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

async fn refused_edits_leave_no_gap_in_seq<B: Backend>(backend: &B) {
    let (world, alice, _bob, chat, hello) = chat_with_one_message(backend).await;
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

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        an_edit_is_an_entry_of_its_own_and_updates_the_message,
        only_the_latest_edit_keeps_its_text,
        a_message_can_be_targeted_by_its_client_msg_id,
        an_edit_retry_returns_the_original_and_changes_nothing,
        an_edit_retry_succeeds_even_after_the_window_has_passed,
        only_the_sender_can_edit,
        the_sender_may_edit_from_another_device,
        the_target_must_be_a_message_of_this_conversation,
        a_target_outside_the_callers_visible_range_is_not_found,
        an_edit_is_allowed_until_the_window_closes,
        the_edit_window_comes_from_the_policy,
        an_invalid_edit_is_rejected_and_uses_no_seq,
        a_blocked_sender_cannot_edit,
        a_client_msg_id_cannot_be_reused_for_another_kind_of_entry,
        an_edit_tells_every_member_to_sync,
        an_edit_moves_the_conversations_updated_at,
        an_edit_leaves_the_senders_read_position_unchanged,
        a_text_send_cannot_reuse_the_client_msg_id_of_an_edit,
        refused_edits_leave_no_gap_in_seq,
    ]
}
