use push_domain::{
    ClientMsgId, Conversation, ConversationId, ConversationKind, DeviceId, DomainError, Entry,
    EntryKind, GroupPositions, Member, Store, Tx, UserId,
};

use super::{add_users, conversation, entry, member, start, store_with};
use crate::{Backend, Case, case_list};

async fn positions_only_move_forward<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();

    assert_eq!(tx.get_cursor(device, id).await, Ok(None));
    tx.create_cursor_if_absent(device, id, 3).await.unwrap();
    tx.create_cursor_if_absent(device, id, 9).await.unwrap();
    assert_eq!(tx.get_cursor(device, id).await, Ok(Some(3)));

    assert!(tx.raise_cursor(device, id, 5).await.unwrap());
    assert!(!tx.raise_cursor(device, id, 5).await.unwrap());
    assert!(!tx.raise_cursor(device, id, 4).await.unwrap());
    assert_eq!(tx.get_cursor(device, id).await, Ok(Some(5)));
}

async fn raising_an_absent_position_creates_it<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();

    assert!(tx.raise_cursor(device, id, 4).await.unwrap());
    assert_eq!(tx.get_cursor(device, id).await.unwrap(), Some(4));
}

async fn delivered_is_the_furthest_position_of_any_device_of_the_user<B: Backend>(backend: &B) {
    let (conversation, other) = (conversation(), conversation());
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let (phone, laptop, bobs) = (DeviceId::new(), DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(other.clone()).await.unwrap();
    for (device, user) in [(phone, alice), (laptop, alice), (bobs, bob)] {
        tx.insert_device(device, user).await.unwrap();
    }
    assert_eq!(tx.delivered_seq(id, alice).await, Ok(0));

    tx.raise_cursor(phone, id, 3).await.unwrap();
    tx.raise_cursor(laptop, id, 5).await.unwrap();
    tx.raise_cursor(bobs, id, 8).await.unwrap();
    tx.raise_cursor(phone, other.id, 9).await.unwrap();
    tx.revoke_device(laptop).await.unwrap();

    assert_eq!(
        tx.delivered_seq(id, alice).await,
        Ok(5),
        "a revoked device's position still counts"
    );
    assert_eq!(tx.delivered_seq(id, bob).await, Ok(8));
    assert_eq!(tx.delivered_seq(id, UserId::new()).await, Ok(0));
}

async fn a_position_needs_a_known_device_and_conversation<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();
    tx.commit().await.unwrap();

    let attempts: [(DeviceId, ConversationId, bool); 4] = [
        (DeviceId::new(), id, false),
        (device, ConversationId::new(), false),
        (DeviceId::new(), id, true),
        (device, ConversationId::new(), true),
    ];
    for (device, conversation, raise) in attempts {
        let mut tx = store.begin().await.unwrap();
        let result = if raise {
            tx.raise_cursor(device, conversation, 1).await.map(|_| ())
        } else {
            tx.create_cursor_if_absent(device, conversation, 0).await
        };
        assert!(matches!(result, Err(DomainError::Store(_))));
    }
}

async fn member_positions_pair_every_member_with_their_furthest_delivery<B: Backend>(backend: &B) {
    let (conversation, other) = (conversation(), conversation());
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
    add_users(&store, &[alice, bob, carol]).await;
    let (phone, laptop, bobs) = (DeviceId::new(), DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(other.clone()).await.unwrap();
    let former = Member {
        left_at_seq: Some(2),
        read_seq: 2,
        ..member(id, carol)
    };
    for row in [member(id, alice), member(id, bob), former.clone()] {
        tx.insert_member(row).await.unwrap();
    }
    tx.insert_member(member(other.id, alice)).await.unwrap();
    for (device, user) in [(phone, alice), (laptop, alice), (bobs, bob)] {
        tx.insert_device(device, user).await.unwrap();
    }
    tx.raise_cursor(phone, id, 3).await.unwrap();
    tx.raise_cursor(laptop, id, 4).await.unwrap();
    tx.raise_cursor(bobs, other.id, 9).await.unwrap();
    tx.revoke_device(laptop).await.unwrap();

    let mut positions = tx.member_positions(id).await.unwrap();
    positions.sort_by_key(|(member, _)| member.user_id);
    let mut expected = vec![(member(id, alice), 4), (member(id, bob), 0), (former, 0)];
    expected.sort_by_key(|(member, _)| member.user_id);
    assert_eq!(positions, expected);
    assert_eq!(tx.member_positions(ConversationId::new()).await, Ok(vec![]));
}

/// Six conversations of alice: the last `seq`, her member row (visible from, left at), her
/// phone's position if it has one, and the position sync should start from, if anything is
/// left to receive. Her laptop has everything; it does not count.
async fn sync_positions_list_memberships_with_something_left_to_receive<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let (phone, laptop) = (DeviceId::new(), DeviceId::new());
    type Row = (i64, i64, Option<i64>, Option<i64>, Option<i64>);
    let rows: [Row; 6] = [
        (5, 1, None, Some(2), Some(2)),    // behind
        (4, 3, None, None, Some(2)),       // no position: starts at the floor
        (5, 1, None, Some(5), None),       // up to date
        (6, 1, Some(3), Some(3), None),    // left, has everything up to leaving
        (6, 1, Some(4), Some(1), Some(1)), // left, behind
        (4, 4, None, Some(1), Some(3)),    // position below the floor
    ];
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(phone, alice).await.unwrap();
    tx.insert_device(laptop, alice).await.unwrap();
    let mut expected = Vec::new();
    for (last_seq, visible_from_seq, left_at_seq, position, starts_at) in rows {
        let conversation = conversation();
        let id = conversation.id;
        tx.insert_conversation(conversation.clone()).await.unwrap();
        for _ in 0..last_seq {
            tx.next_seq(id, start()).await.unwrap();
        }
        let row = Member {
            visible_from_seq,
            left_at_seq,
            ..member(id, alice)
        };
        tx.insert_member(row.clone()).await.unwrap();
        tx.insert_member(member(id, bob)).await.unwrap();
        if let Some(position) = position {
            tx.raise_cursor(phone, id, position).await.unwrap();
        }
        tx.raise_cursor(laptop, id, last_seq).await.unwrap();
        if let Some(starts_at) = starts_at {
            let stored = Conversation {
                last_seq,
                ..conversation
            };
            expected.push((stored, row, starts_at));
        }
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    let mut positions = tx.sync_positions(alice, phone).await.unwrap();
    positions.sort_by_key(|(conversation, _, _)| conversation.id);
    expected.sort_by_key(|(conversation, _, _)| conversation.id);
    assert_eq!(positions, expected);
    assert_eq!(tx.sync_positions(alice, laptop).await, Ok(vec![]));
}

/// A group log with seven entries and five members:
///
/// | seq | entry |
/// |---|---|
/// | 1 | system entry by alice |
/// | 2 | message from alice |
/// | 3 | message from bob |
/// | 4 | message from carol, deleted |
/// | 5 | edit by alice of seq 2 |
/// | 6 | message from alice |
/// | 7 | message from bob |
///
/// alice has read 2 and her device has 7; bob has read 0, his revoked phone has 6 and his
/// laptop 1; carol has read 5 and has no device; dave left at 3; erin joined at 6 and her
/// device still has 3 from an earlier membership.
async fn group_positions_are_minimums_of_effective_positions_over_current_members<B: Backend>(
    backend: &B,
) {
    let group = Conversation {
        kind: ConversationKind::Group,
        ..conversation()
    };
    let id = group.id;
    let store = store_with(backend, group).await;
    let [alice, bob, carol, dave, erin] = [(); 5].map(|()| UserId::new());
    add_users(&store, &[alice, bob, carol, dave, erin]).await;
    let mut tx = store.begin().await.unwrap();
    let senders = [alice, alice, bob, carol, alice, alice, bob];
    for (seq, sender) in (1..).zip(senders) {
        let mut entry = Entry {
            sender_id: sender,
            ..entry(id, seq, ClientMsgId::new())
        };
        match seq {
            1 => entry.kind = EntryKind::System,
            4 => entry.deleted_at = Some(start()),
            5 => {
                entry.kind = EntryKind::Edit;
                entry.target_seq = Some(2);
            }
            _ => {}
        }
        tx.insert_entry(entry).await.unwrap();
    }
    let rows = [
        (alice, 1, None, 2),
        (bob, 1, None, 0),
        (carol, 1, None, 5),
        (dave, 1, Some(3), 0),
        (erin, 6, None, 0),
    ];
    for (user, visible_from_seq, left_at_seq, read_seq) in rows {
        tx.insert_member(Member {
            visible_from_seq,
            left_at_seq,
            read_seq,
            ..member(id, user)
        })
        .await
        .unwrap();
    }
    let devices = [(alice, 7), (bob, 6), (bob, 1), (erin, 3)];
    let mut ids = Vec::new();
    for (user, position) in devices {
        let device = DeviceId::new();
        tx.insert_device(device, user).await.unwrap();
        tx.raise_cursor(device, id, position).await.unwrap();
        ids.push(device);
    }
    tx.revoke_device(ids[1]).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.group_positions(id, 7).await,
        Ok(Some(GroupPositions {
            delivered: 5,
            read: 1
        }))
    );
    assert_eq!(
        tx.group_positions(id, 4).await,
        Ok(Some(GroupPositions {
            delivered: 4,
            read: 1
        })),
        "positions are capped at the end of the log"
    );
}

async fn a_group_without_current_members_has_no_positions<B: Backend>(backend: &B) {
    let group = Conversation {
        kind: ConversationKind::Group,
        ..conversation()
    };
    let id = group.id;
    let store = store_with(backend, group).await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.group_positions(id, 0).await, Ok(None));

    tx.insert_member(Member {
        left_at_seq: Some(0),
        ..member(id, alice)
    })
    .await
    .unwrap();
    assert_eq!(tx.group_positions(id, 0).await, Ok(None));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        positions_only_move_forward,
        raising_an_absent_position_creates_it,
        delivered_is_the_furthest_position_of_any_device_of_the_user,
        member_positions_pair_every_member_with_their_furthest_delivery,
        sync_positions_list_memberships_with_something_left_to_receive,
        a_position_needs_a_known_device_and_conversation,
        group_positions_are_minimums_of_effective_positions_over_current_members,
        a_group_without_current_members_has_no_positions,
    ]
}
