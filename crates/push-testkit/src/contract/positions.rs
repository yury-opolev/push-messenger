use push_domain::{ConversationId, DeviceId, DomainError, Store, Tx, UserId};

use super::{add_users, conversation, store_with};
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

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        positions_only_move_forward,
        raising_an_absent_position_creates_it,
        delivered_is_the_furthest_position_of_any_device_of_the_user,
        a_position_needs_a_known_device_and_conversation,
    ]
}
