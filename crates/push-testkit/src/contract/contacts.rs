use push_domain::{ContactState, DomainError, InsertOutcome, Store, Tx, UserId};

use super::{add_users, invite, start};
use crate::{Backend, Case, case_list};

async fn contact_rows_are_per_direction_and_replaced_by_put<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
    add_users(&store, &[alice, bob, carol]).await;
    let mut tx = store.begin().await.unwrap();
    tx.lock_contact_pair(alice, bob).await.unwrap();
    tx.lock_contact_pair(bob, alice).await.unwrap();
    tx.put_contact(alice, bob, ContactState::RequestedOut)
        .await
        .unwrap();
    tx.put_contact(alice, bob, ContactState::Accepted)
        .await
        .unwrap();
    tx.put_contact(alice, carol, ContactState::Blocked)
        .await
        .unwrap();
    tx.put_contact(bob, alice, ContactState::Accepted)
        .await
        .unwrap();

    assert_eq!(
        tx.get_contact(alice, bob).await,
        Ok(Some(ContactState::Accepted))
    );
    assert_eq!(tx.get_contact(carol, alice).await, Ok(None));
    let mut rows = tx.contacts_of(alice).await.unwrap();
    rows.sort_by_key(|(peer, _)| *peer);
    let mut expected = vec![
        (bob, ContactState::Accepted),
        (carol, ContactState::Blocked),
    ];
    expected.sort_by_key(|(peer, _)| *peer);
    assert_eq!(rows, expected);
    assert_eq!(tx.contacts_of(UserId::new()).await, Ok(vec![]));
}

async fn deleting_a_contact_removes_one_direction<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut tx = store.begin().await.unwrap();
    tx.put_contact(alice, bob, ContactState::Accepted)
        .await
        .unwrap();
    tx.put_contact(bob, alice, ContactState::Accepted)
        .await
        .unwrap();

    tx.delete_contact(alice, bob).await.unwrap();
    tx.delete_contact(alice, bob).await.unwrap();
    assert_eq!(tx.get_contact(alice, bob).await, Ok(None));
    assert_eq!(
        tx.get_contact(bob, alice).await,
        Ok(Some(ContactState::Accepted))
    );
}

async fn a_contact_with_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.put_contact(alice, UserId::new(), ContactState::RequestedOut)
            .await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.put_contact(UserId::new(), alice, ContactState::RequestedOut)
            .await,
        Err(DomainError::Store(_))
    ));
}

async fn an_invite_code_is_stored_once<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (first, second) = (UserId::new(), UserId::new());
    add_users(&store, &[first, second]).await;
    let mut tx = store.begin().await.unwrap();

    assert_eq!(
        tx.insert_invite(invite("ABCDEFGHJK", first)).await,
        Ok(InsertOutcome::Inserted)
    );
    assert_eq!(
        tx.insert_invite(invite("ABCDEFGHJK", second)).await,
        Ok(InsertOutcome::Duplicate)
    );
    assert_eq!(
        tx.lock_invite("ABCDEFGHJK").await,
        Ok(Some(invite("ABCDEFGHJK", first)))
    );
    assert_eq!(tx.lock_invite("0000000000").await, Ok(None));
}

async fn an_invite_of_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_invite(invite("0000000000", UserId::new())).await,
        Err(DomainError::Store(_))
    ));
}

async fn update_invite_changes_only_uses_and_revocation<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_invite(invite("ABCDEFGHJK", user)).await.unwrap();

    let mut changed = invite("ABCDEFGHJK", UserId::new());
    changed.uses = 1;
    changed.revoked_at = Some(start());
    changed.max_uses = 9;
    tx.update_invite(changed).await.unwrap();

    let stored = tx.lock_invite("ABCDEFGHJK").await.unwrap().unwrap();
    assert_eq!((stored.uses, stored.revoked_at), (1, Some(start())));
    assert_eq!((stored.user_id, stored.max_uses), (user, 1));
    assert!(matches!(
        tx.update_invite(invite("0000000000", user)).await,
        Err(DomainError::Store(_))
    ));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        contact_rows_are_per_direction_and_replaced_by_put,
        deleting_a_contact_removes_one_direction,
        a_contact_with_an_unknown_user_is_refused,
        an_invite_code_is_stored_once,
        an_invite_of_an_unknown_user_is_refused,
        update_invite_changes_only_uses_and_revocation,
    ]
}
