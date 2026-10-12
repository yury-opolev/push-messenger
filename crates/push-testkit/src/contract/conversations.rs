use chrono::TimeDelta;
use push_domain::{
    Conversation, ConversationId, ConversationKind, DomainError, InsertOutcome, Member, Role,
    Store, Tx, UserId,
};

use super::{add_users, conversation, member, start, store_with};
use crate::{Backend, Case, case_list};

async fn a_conversation_is_read_back_as_stored<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut group = conversation();
    group.kind = ConversationKind::Group;
    group.title = Some("Trip".into());
    let mut direct = conversation();
    direct.direct_key = Some([9; 32]);
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(group.clone()).await.unwrap();
    tx.insert_conversation(direct.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.get_conversation(group.id).await, Ok(Some(group)));
    assert_eq!(
        tx.get_conversation(direct.id).await,
        Ok(Some(direct.clone()))
    );
    assert_eq!(tx.get_conversation(ConversationId::new()).await, Ok(None));
    assert_eq!(tx.find_direct([9; 32]).await, Ok(Some(direct.id)));
    assert_eq!(tx.find_direct([8; 32]).await, Ok(None));
}

async fn next_seq_counts_from_one_and_a_rollback_gives_the_number_back<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.next_seq(id, start()).await.unwrap(), 1);
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
}

async fn next_seq_moves_updated_at_and_a_rollback_restores_it<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (first, second) = (start() + TimeDelta::hours(1), start() + TimeDelta::hours(2));

    let mut tx = store.begin().await.unwrap();
    tx.next_seq(id, first).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    tx.next_seq(id, second).await.unwrap();
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    let stored = tx.get_conversation(id).await.unwrap().unwrap();
    assert_eq!((stored.last_seq, stored.updated_at), (1, first));
}

async fn next_seq_never_moves_updated_at_back<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let later = start() + TimeDelta::hours(1);

    let mut tx = store.begin().await.unwrap();
    tx.next_seq(id, later).await.unwrap();
    tx.next_seq(id, start()).await.unwrap();

    let stored = tx.get_conversation(id).await.unwrap().unwrap();
    assert_eq!((stored.last_seq, stored.updated_at), (2, later));
}

async fn a_second_direct_conversation_for_the_same_pair_is_a_duplicate<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut first = conversation();
    first.direct_key = Some([7; 32]);
    let mut second = conversation();
    second.direct_key = Some([7; 32]);
    let second_id = second.id;

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.insert_conversation(first.clone()).await.unwrap(),
        InsertOutcome::Inserted
    );
    assert_eq!(
        tx.insert_conversation(second).await.unwrap(),
        InsertOutcome::Duplicate
    );
    assert_eq!(tx.find_direct([7; 32]).await.unwrap(), Some(first.id));
    assert_eq!(tx.get_conversation(second_id).await.unwrap(), None);
}

async fn a_conversation_id_cannot_be_inserted_twice<B: Backend>(backend: &B) {
    let first = conversation();
    let mut second = conversation();
    second.id = first.id;
    second.direct_key = Some([3; 32]);
    let store = store_with(backend, first).await;

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_conversation(second).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.find_direct([3; 32]).await.unwrap(), None);
}

async fn set_title_changes_only_the_title<B: Backend>(backend: &B) {
    let original = conversation();
    let id = original.id;
    let store = store_with(backend, original.clone()).await;
    let mut tx = store.begin().await.unwrap();

    tx.set_title(id, "Trip".into()).await.unwrap();
    let stored = tx.get_conversation(id).await.unwrap().unwrap();
    assert_eq!(
        stored,
        Conversation {
            title: Some("Trip".into()),
            ..original
        }
    );
    assert_eq!(
        tx.set_title(ConversationId::new(), "Trip".into()).await,
        Err(DomainError::ConversationNotFound)
    );
}

async fn operations_on_missing_rows_fail_as_documented<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.next_seq(ConversationId::new(), start()).await,
        Err(DomainError::ConversationNotFound)
    );
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.raise_read_seq(ConversationId::new(), UserId::new(), 1)
            .await,
        Err(DomainError::NotMember)
    );
}

async fn inserting_a_member_again_replaces_the_row<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_member(member(id, alice)).await.unwrap();
    tx.insert_member(member(id, bob)).await.unwrap();

    let changed = Member {
        role: Role::Admin,
        visible_from_seq: 4,
        left_at_seq: Some(9),
        read_seq: 6,
        ..member(id, alice)
    };
    tx.insert_member(changed.clone()).await.unwrap();
    assert_eq!(tx.get_member(id, alice).await, Ok(Some(changed.clone())));
    assert_eq!(tx.get_member(id, UserId::new()).await, Ok(None));
    let mut members = tx.members(id).await.unwrap();
    members.sort_by_key(|member| member.user_id);
    let mut expected = vec![changed, member(id, bob)];
    expected.sort_by_key(|member| member.user_id);
    assert_eq!(members, expected);
    assert_eq!(tx.members(ConversationId::new()).await, Ok(vec![]));
}

async fn a_member_needs_a_known_user_and_conversation<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_member(member(id, UserId::new())).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_member(member(ConversationId::new(), alice)).await,
        Err(DomainError::Store(_))
    ));
}

async fn memberships_pair_each_membership_with_its_current_conversation<B: Backend>(backend: &B) {
    let (first, second, elsewhere) = (conversation(), conversation(), conversation());
    let store = store_with(backend, first.clone()).await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(second.clone()).await.unwrap();
    tx.insert_conversation(elsewhere.clone()).await.unwrap();
    tx.insert_member(member(first.id, alice)).await.unwrap();
    let left = Member {
        left_at_seq: Some(1),
        ..member(second.id, alice)
    };
    tx.insert_member(left.clone()).await.unwrap();
    tx.insert_member(member(elsewhere.id, bob)).await.unwrap();
    tx.next_seq(first.id, start()).await.unwrap();

    let mut memberships = tx.memberships(alice).await.unwrap();
    memberships.sort_by_key(|(conversation, _)| conversation.id);
    let mut expected = vec![
        (
            Conversation {
                last_seq: 1,
                ..first.clone()
            },
            member(first.id, alice),
        ),
        (second, left),
    ];
    expected.sort_by_key(|(conversation, _)| conversation.id);
    assert_eq!(memberships, expected);
    assert_eq!(tx.memberships(UserId::new()).await, Ok(vec![]));
}

async fn the_read_position_only_moves_forward<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_member(member(id, alice)).await.unwrap();

    assert_eq!(tx.raise_read_seq(id, alice, 3).await, Ok(true));
    assert_eq!(tx.raise_read_seq(id, alice, 3).await, Ok(false));
    assert_eq!(tx.raise_read_seq(id, alice, 2).await, Ok(false));
    assert_eq!(tx.get_member(id, alice).await.unwrap().unwrap().read_seq, 3);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_conversation_is_read_back_as_stored,
        next_seq_counts_from_one_and_a_rollback_gives_the_number_back,
        next_seq_moves_updated_at_and_a_rollback_restores_it,
        next_seq_never_moves_updated_at_back,
        a_second_direct_conversation_for_the_same_pair_is_a_duplicate,
        a_conversation_id_cannot_be_inserted_twice,
        set_title_changes_only_the_title,
        operations_on_missing_rows_fail_as_documented,
        inserting_a_member_again_replaces_the_row,
        a_member_needs_a_known_user_and_conversation,
        memberships_pair_each_membership_with_its_current_conversation,
        the_read_position_only_moves_forward,
    ]
}
