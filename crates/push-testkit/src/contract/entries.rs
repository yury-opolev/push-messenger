use chrono::TimeDelta;
use push_domain::{
    ClientMsgId, ContentType, ConversationId, DeviceId, DomainError, Entry, EntryKind,
    InsertOutcome, MessageId, Store, Tx, UserId,
};

use super::{conversation, entry, seqs, start, store_with};
use crate::{Backend, Case, case_list};

async fn a_reused_client_msg_id_is_a_duplicate<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let client_msg_id = ClientMsgId::new();
    let mut tx = store.begin().await.unwrap();

    let first = entry(id, 1, client_msg_id);
    assert_eq!(
        tx.insert_entry(first.clone()).await.unwrap(),
        InsertOutcome::Inserted
    );
    assert_eq!(
        tx.insert_entry(entry(id, 2, client_msg_id)).await.unwrap(),
        InsertOutcome::Duplicate
    );
    assert_eq!(
        tx.entry_by_client_id(id, client_msg_id).await,
        Ok(Some(first))
    );
    assert_eq!(
        tx.entry_by_client_id(id, ClientMsgId::new()).await,
        Ok(None)
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 0, 5, 0, 10).await.unwrap()),
        vec![1]
    );
}

async fn every_field_of_an_entry_is_read_back<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let edit = Entry {
        kind: EntryKind::Edit,
        target_seq: Some(1),
        sender_device_id: Some(DeviceId::new()),
        content_type: ContentType::SystemEvent,
        body: None,
        client_ts: Some(start() - TimeDelta::seconds(3)),
        edited_at: Some(start() + TimeDelta::minutes(1)),
        edit_seq: Some(7),
        deleted_at: Some(start() + TimeDelta::minutes(2)),
        ..entry(id, 2, ClientMsgId::new())
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_entry(entry(id, 1, ClientMsgId::new()))
        .await
        .unwrap();
    tx.insert_entry(edit.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.entry_by_message_id(id, edit.message_id).await,
        Ok(Some(edit))
    );
}

async fn entry_ranges_respect_their_bounds<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    for seq in 1..=5 {
        tx.insert_entry(entry(id, seq, ClientMsgId::new()))
            .await
            .unwrap();
    }

    assert_eq!(
        seqs(&tx.entries_after(id, 1, 4, 0, 10).await.unwrap()),
        vec![2, 3, 4]
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 1, 5, 0, 2).await.unwrap()),
        vec![2, 3]
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 5, 5, 0, 10).await.unwrap()),
        Vec::<i64>::new()
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 1, 5, 2).await.unwrap()),
        vec![3, 4]
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 2, 3, 10).await.unwrap()),
        vec![2]
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 1, 1, 10).await.unwrap()),
        Vec::<i64>::new()
    );
}

async fn only_live_messages_from_others_are_counted_up_to_the_cap<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let me = UserId::new();
    let mut tx = store.begin().await.unwrap();
    let mut entries = vec![
        entry(id, 1, ClientMsgId::new()),
        Entry {
            sender_id: me,
            ..entry(id, 2, ClientMsgId::new())
        },
        Entry {
            deleted_at: Some(start()),
            ..entry(id, 3, ClientMsgId::new())
        },
        Entry {
            kind: EntryKind::Edit,
            ..entry(id, 4, ClientMsgId::new())
        },
    ];
    entries.extend((5..=7).map(|seq| entry(id, seq, ClientMsgId::new())));
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }

    assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 100).await, Ok(4));
    assert_eq!(tx.count_incoming_messages(id, me, 1, 4, 100).await, Ok(0));
    assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 2).await, Ok(2));
    assert_eq!(tx.count_incoming_messages(id, me, 7, 7, 100).await, Ok(0));
}

async fn messages_before_skips_edits_and_deletes<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    let kinds = [
        EntryKind::Message,
        EntryKind::Edit,
        EntryKind::System,
        EntryKind::Delete,
        EntryKind::Message,
    ];
    for (seq, kind) in (1..).zip(kinds) {
        tx.insert_entry(Entry {
            kind,
            ..entry(id, seq, ClientMsgId::new())
        })
        .await
        .unwrap();
    }

    assert_eq!(
        seqs(&tx.messages_before(id, 1, 6, 2).await.unwrap()),
        vec![3, 5]
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 1, 6, 10).await.unwrap()),
        vec![1, 3, 5]
    );
}

async fn an_entry_of_an_unknown_conversation_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_entry(entry(ConversationId::new(), 1, ClientMsgId::new()))
            .await,
        Err(DomainError::Store(_))
    ));
}

async fn a_message_id_cannot_be_inserted_twice<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let first = entry(id, 1, ClientMsgId::new());
    let mut second = entry(id, 2, ClientMsgId::new());
    second.message_id = first.message_id;
    let mut tx = store.begin().await.unwrap();
    tx.insert_entry(first).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_entry(second).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        seqs(&tx.entries_after(id, 0, 5, 0, 10).await.unwrap()),
        vec![1]
    );
}

async fn an_entry_cannot_overwrite_an_existing_seq<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_entry(entry(id, 1, ClientMsgId::new()))
        .await
        .unwrap();

    assert!(matches!(
        tx.insert_entry(entry(id, 1, ClientMsgId::new())).await,
        Err(DomainError::Store(_))
    ));
}

async fn an_entry_is_found_by_message_id_only_in_its_own_conversation<B: Backend>(backend: &B) {
    let (home, elsewhere) = (conversation(), conversation());
    let (home_id, elsewhere_id) = (home.id, elsewhere.id);
    let store = store_with(backend, home).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(elsewhere).await.unwrap();
    let stored = entry(home_id, 1, ClientMsgId::new());
    tx.insert_entry(stored.clone()).await.unwrap();

    assert_eq!(
        tx.entry_by_message_id(home_id, stored.message_id)
            .await
            .unwrap(),
        Some(stored.clone())
    );
    assert_eq!(
        tx.entry_by_message_id(elsewhere_id, stored.message_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        tx.entry_by_message_id(home_id, MessageId::new())
            .await
            .unwrap(),
        None
    );
}

async fn update_entry_changes_only_the_mutable_fields<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    let original = Entry {
        content_type: ContentType::Rich,
        ..entry(id, 1, ClientMsgId::new())
    };
    tx.insert_entry(original.clone()).await.unwrap();

    let mut changed = original.clone();
    changed.body = None;
    changed.edited_at = Some(start() + TimeDelta::hours(1));
    changed.edit_seq = Some(4);
    changed.deleted_at = Some(start() + TimeDelta::hours(2));
    changed.answer_seq = Some(5);
    changed.sender_id = UserId::new();
    changed.kind = EntryKind::Edit;
    changed.content_type = ContentType::Text;
    tx.update_entry(changed.clone()).await.unwrap();

    let stored = tx
        .entry_by_message_id(id, original.message_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.body, None);
    assert_eq!(stored.edited_at, changed.edited_at);
    assert_eq!(stored.edit_seq, Some(4));
    assert_eq!(stored.deleted_at, changed.deleted_at);
    assert_eq!(stored.answer_seq, Some(5));
    assert_eq!(stored.sender_id, original.sender_id);
    assert_eq!(stored.kind, EntryKind::Message);
    assert_eq!(stored.content_type, ContentType::Rich);
}

async fn a_card_is_answered_once_by_a_later_entry<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let text = entry(id, 1, ClientMsgId::new());
    let card = Entry {
        content_type: ContentType::Rich,
        ..entry(id, 2, ClientMsgId::new())
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_entry(text.clone()).await.unwrap();
    tx.insert_entry(card.clone()).await.unwrap();
    tx.commit().await.unwrap();

    // Each refusal ends its transaction.
    for (target, answer_seq) in [(&text, Some(3)), (&card, Some(2)), (&card, Some(1))] {
        let mut tx = store.begin().await.unwrap();
        let mut changed = target.clone();
        changed.answer_seq = answer_seq;
        assert!(
            matches!(tx.update_entry(changed).await, Err(DomainError::Store(_))),
            "seq {}, answer_seq {answer_seq:?}",
            target.seq
        );
    }

    let mut answered = card.clone();
    answered.answer_seq = Some(3);
    let mut tx = store.begin().await.unwrap();
    tx.update_entry(answered.clone()).await.unwrap();
    tx.commit().await.unwrap();
    for answer_seq in [Some(4), None] {
        let mut tx = store.begin().await.unwrap();
        let mut changed = answered.clone();
        changed.answer_seq = answer_seq;
        assert!(
            matches!(tx.update_entry(changed).await, Err(DomainError::Store(_))),
            "answer_seq {answer_seq:?}"
        );
    }
    // Keeping the answer, an answered card can still be deleted.
    let mut deleted = answered.clone();
    deleted.body = None;
    deleted.deleted_at = Some(start() + TimeDelta::hours(1));
    let mut tx = store.begin().await.unwrap();
    tx.update_entry(deleted.clone()).await.unwrap();
    assert_eq!(
        tx.entry_by_message_id(id, card.message_id).await,
        Ok(Some(deleted))
    );
}

async fn updating_a_missing_entry_fails<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();

    assert!(matches!(
        tx.update_entry(entry(id, 1, ClientMsgId::new())).await,
        Err(DomainError::Store(_))
    ));
}

async fn erasing_edit_bodies_touches_only_edits_of_that_target<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    let edit = |seq, target| Entry {
        kind: EntryKind::Edit,
        target_seq: Some(target),
        ..entry(id, seq, ClientMsgId::new())
    };
    tx.insert_entry(entry(id, 1, ClientMsgId::new()))
        .await
        .unwrap();
    tx.insert_entry(entry(id, 2, ClientMsgId::new()))
        .await
        .unwrap();
    tx.insert_entry(edit(3, 1)).await.unwrap();
    tx.insert_entry(edit(4, 2)).await.unwrap();
    tx.insert_entry(edit(5, 1)).await.unwrap();

    tx.erase_edit_bodies(id, 1).await.unwrap();
    let bodies: Vec<bool> = tx
        .entries_after(id, 0, 5, 0, 10)
        .await
        .unwrap()
        .iter()
        .map(|entry| entry.body.is_some())
        .collect();
    assert_eq!(bodies, [true, true, false, true, false]);
}

async fn entries_after_skips_changes_to_messages_below_targets_from<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    for seq in 1..=3 {
        tx.insert_entry(entry(id, seq, ClientMsgId::new()))
            .await
            .unwrap();
    }
    for (seq, target) in [(4, 1), (5, 3), (6, 2)] {
        tx.insert_entry(Entry {
            kind: EntryKind::Edit,
            target_seq: Some(target),
            ..entry(id, seq, ClientMsgId::new())
        })
        .await
        .unwrap();
    }

    assert_eq!(
        seqs(&tx.entries_after(id, 2, 6, 3, 10).await.unwrap()),
        vec![3, 5]
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 0, 6, 2, 3).await.unwrap()),
        vec![1, 2, 3],
        "the limit counts only returned entries"
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 3, 6, 2, 2).await.unwrap()),
        vec![5, 6]
    );
}

async fn the_first_live_message_skips_changes_deleted_messages_and_the_excluded_sender<
    B: Backend,
>(
    backend: &B,
) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let me = UserId::new();
    let mut tx = store.begin().await.unwrap();
    let entries = [
        entry(id, 1, ClientMsgId::new()),
        Entry {
            sender_id: me,
            ..entry(id, 2, ClientMsgId::new())
        },
        Entry {
            deleted_at: Some(start()),
            ..entry(id, 3, ClientMsgId::new())
        },
        Entry {
            kind: EntryKind::Edit,
            ..entry(id, 4, ClientMsgId::new())
        },
        entry(id, 5, ClientMsgId::new()),
    ];
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }

    assert_eq!(tx.first_live_message(id, 0, 5, Some(me)).await, Ok(Some(1)));
    assert_eq!(tx.first_live_message(id, 1, 5, Some(me)).await, Ok(Some(5)));
    assert_eq!(tx.first_live_message(id, 1, 5, None).await, Ok(Some(2)));
    assert_eq!(tx.first_live_message(id, 2, 4, None).await, Ok(None));
    assert_eq!(tx.first_live_message(id, 5, 5, None).await, Ok(None));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_reused_client_msg_id_is_a_duplicate,
        every_field_of_an_entry_is_read_back,
        entry_ranges_respect_their_bounds,
        only_live_messages_from_others_are_counted_up_to_the_cap,
        messages_before_skips_edits_and_deletes,
        an_entry_of_an_unknown_conversation_is_refused,
        a_message_id_cannot_be_inserted_twice,
        an_entry_cannot_overwrite_an_existing_seq,
        an_entry_is_found_by_message_id_only_in_its_own_conversation,
        update_entry_changes_only_the_mutable_fields,
        a_card_is_answered_once_by_a_later_entry,
        updating_a_missing_entry_fails,
        erasing_edit_bodies_touches_only_edits_of_that_target,
        entries_after_skips_changes_to_messages_below_targets_from,
        the_first_live_message_skips_changes_deleted_messages_and_the_excluded_sender,
    ]
}
