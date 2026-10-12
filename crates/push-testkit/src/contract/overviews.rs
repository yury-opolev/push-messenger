use chrono::TimeDelta;
use push_domain::{
    ClientMsgId, Conversation, ConversationKind, ConversationOverview, DeviceId, Entry, EntryKind,
    GroupPositions, Member, Role, Store, Tx, UserId,
};

use super::{add_users, conversation, entry, member, start};
use crate::{Backend, Case, case_list};

/// Appends entries from these senders to `conversation`, moving `updated_at` to `at`.
async fn append<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    senders: &[UserId],
    at: chrono::DateTime<chrono::Utc>,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for sender in senders {
        let seq = tx.next_seq(conversation.id, at).await.unwrap();
        let entry = Entry {
            sender_id: *sender,
            ..entry(conversation.id, seq, ClientMsgId::new())
        };
        entries.push(entry);
    }
    entries
}

/// Alice is in three conversations: a direct one with bob, a group with carol (and dave,
/// who left), and a group she left.
async fn overviews_hold_what_a_listing_needs_for_every_membership<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let [alice, bob, carol, dave] = [(); 4].map(|()| UserId::new());
    add_users(&store, &[alice, bob, carol, dave]).await;
    let (bobs_phone, bobs_laptop) = (DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(bobs_phone, bob).await.unwrap();
    tx.insert_device(bobs_laptop, bob).await.unwrap();

    // Direct: bob, alice, bob (deleted), bob, bob. Alice read 1, bob read 2.
    let direct = conversation();
    tx.insert_conversation(direct.clone()).await.unwrap();
    let at = start() + TimeDelta::hours(1);
    let mut entries = append(&mut tx, &direct, &[bob, alice, bob, bob, bob], at).await;
    entries[2].deleted_at = Some(start());
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }
    let alice_direct = Member {
        read_seq: 1,
        ..member(direct.id, alice)
    };
    let bob_direct = Member {
        read_seq: 2,
        ..member(direct.id, bob)
    };
    tx.insert_member(alice_direct.clone()).await.unwrap();
    tx.insert_member(bob_direct.clone()).await.unwrap();
    tx.raise_cursor(bobs_phone, direct.id, 3).await.unwrap();
    tx.raise_cursor(bobs_laptop, direct.id, 1).await.unwrap();
    let direct = Conversation {
        last_seq: 5,
        updated_at: at,
        ..direct
    };

    // Group: system entry by alice, then carol, alice. Dave left at 2.
    let group = Conversation {
        kind: ConversationKind::Group,
        title: Some("Trip".into()),
        ..conversation()
    };
    tx.insert_conversation(group.clone()).await.unwrap();
    let at = start() + TimeDelta::hours(2);
    let mut entries = append(&mut tx, &group, &[alice, carol, alice], at).await;
    entries[0].kind = EntryKind::System;
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }
    let alice_group = Member {
        role: Role::Admin,
        ..member(group.id, alice)
    };
    tx.insert_member(alice_group.clone()).await.unwrap();
    tx.insert_member(Member {
        read_seq: 3,
        ..member(group.id, carol)
    })
    .await
    .unwrap();
    tx.insert_member(Member {
        role: Role::Admin,
        left_at_seq: Some(2),
        ..member(group.id, dave)
    })
    .await
    .unwrap();
    let group = Conversation {
        last_seq: 3,
        updated_at: at,
        ..group
    };

    // Left: bob's system entry, alice leaving at 2 (half an hour in), then bob.
    let left = Conversation {
        kind: ConversationKind::Group,
        ..conversation()
    };
    tx.insert_conversation(left.clone()).await.unwrap();
    let at = start() + TimeDelta::hours(3);
    let mut entries = append(&mut tx, &left, &[bob, alice, bob], at).await;
    entries[0].kind = EntryKind::System;
    entries[1].kind = EntryKind::System;
    entries[1].server_ts = start() + TimeDelta::minutes(30);
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }
    let alice_left = Member {
        left_at_seq: Some(2),
        ..member(left.id, alice)
    };
    tx.insert_member(alice_left.clone()).await.unwrap();
    tx.insert_member(Member {
        role: Role::Admin,
        ..member(left.id, bob)
    })
    .await
    .unwrap();
    let left = Conversation {
        last_seq: 3,
        updated_at: at,
        ..left
    };
    tx.commit().await.unwrap();

    let sorted = |mut users: Vec<UserId>| {
        users.sort();
        users
    };
    let ids = [direct.id, group.id, left.id];
    let direct_overview = ConversationOverview {
        conversation: direct,
        member: alice_direct,
        members: sorted(vec![alice, bob]),
        admins: vec![],
        peer: Some((bob_direct, 3)),
        unread: 2,
        group: None,
        left_at: None,
    };
    let group_overview = ConversationOverview {
        conversation: group,
        member: alice_group,
        members: sorted(vec![alice, carol]),
        admins: vec![alice],
        peer: None,
        unread: 1,
        group: Some(GroupPositions {
            delivered: 1,
            read: 1,
        }),
        left_at: None,
    };
    let left_overview = ConversationOverview {
        conversation: left,
        member: alice_left,
        members: vec![bob],
        admins: vec![bob],
        peer: None,
        unread: 0,
        group: Some(GroupPositions {
            delivered: 3,
            read: 3,
        }),
        left_at: Some(start() + TimeDelta::minutes(30)),
    };
    let by_id = |mut overviews: Vec<ConversationOverview>| {
        overviews.sort_by_key(|overview| overview.conversation.id);
        overviews
    };

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.conversation_overviews(alice, None, 100).await.map(by_id),
        Ok(by_id(vec![
            direct_overview.clone(),
            group_overview.clone(),
            left_overview.clone()
        ]))
    );
    assert_eq!(
        tx.conversation_overviews(alice, Some(start() + TimeDelta::minutes(90)), 100)
            .await
            .map(by_id),
        Ok(by_id(vec![group_overview, left_overview])),
        "only conversations changed after the given time"
    );
    let capped = by_id(tx.conversation_overviews(alice, None, 1).await.unwrap());
    let unread: Vec<_> = capped
        .iter()
        .map(|overview| (overview.conversation.id, overview.unread))
        .collect();
    let mut expected = vec![(ids[0], 1), (ids[1], 1), (ids[2], 0)];
    expected.sort();
    assert_eq!(unread, expected, "unread counts stop at the cap");
    assert_eq!(
        tx.conversation_overviews(UserId::new(), None, 100).await,
        Ok(vec![])
    );
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![overviews_hold_what_a_listing_needs_for_every_membership]
}
