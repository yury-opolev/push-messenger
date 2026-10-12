use push_domain::{Store, Tx, UserId};

use crate::{Backend, Case, case_list};

async fn committed_changes_are_visible_to_later_transactions<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_user(user).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert!(tx.user_exists(user).await.unwrap());
}

async fn rolled_back_and_dropped_transactions_leave_no_trace<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (rolled_back, dropped) = (UserId::new(), UserId::new());

    let mut tx = store.begin().await.unwrap();
    tx.insert_user(rolled_back).await.unwrap();
    tx.rollback().await.unwrap();
    {
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(dropped).await.unwrap();
    }

    let mut tx = store.begin().await.unwrap();
    assert!(!tx.user_exists(rolled_back).await.unwrap());
    assert!(!tx.user_exists(dropped).await.unwrap());
}

async fn a_transaction_sees_its_own_changes<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_user(user).await.unwrap();

    assert!(tx.user_exists(user).await.unwrap());
    tx.rollback().await.unwrap();
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        committed_changes_are_visible_to_later_transactions,
        rolled_back_and_dropped_transactions_leave_no_trace,
        a_transaction_sees_its_own_changes,
    ]
}
