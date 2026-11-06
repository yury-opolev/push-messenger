//! Conversation flags: per-user hiding of direct conversations and deleted users.

use chrono::TimeDelta;
use push_domain::{
    ConversationFlags, ConversationId, ConversationKind, DomainError, Store, Tx, UserId,
};

use super::{add_users, conversation, member, start};
use crate::{Backend, Case, case_list};

/// A store with Alice and Bob in a direct conversation, and Carol, who is in none.
async fn direct<B: Backend>(backend: &B) -> (B::Store, ConversationId, [UserId; 3]) {
    let store = backend.store().await;
    let users = [UserId::new(), UserId::new(), UserId::new()];
    add_users(&store, &users).await;
    let chat = conversation();
    let id = chat.id;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(chat).await.unwrap();
    tx.insert_member(member(id, users[0])).await.unwrap();
    tx.insert_member(member(id, users[1])).await.unwrap();
    tx.commit().await.unwrap();
    (store, id, users)
}

async fn flags<S: Store>(store: &S, user: UserId) -> Vec<ConversationFlags> {
    let mut tx = store.begin().await.unwrap();
    let flags = tx.conversation_flags(user).await.unwrap();
    tx.rollback().await.unwrap();
    flags
}

async fn hiding_is_per_user_and_reports_changes<B: Backend>(backend: &B) {
    let (store, chat, [alice, bob, _]) = direct(backend).await;
    let later = start() + TimeDelta::minutes(5);
    let mut tx = store.begin().await.unwrap();
    // Showing what was never hidden changes nothing.
    assert_eq!(
        tx.set_conversation_hidden(chat, alice, false, start())
            .await,
        Ok(false)
    );
    assert_eq!(
        tx.set_conversation_hidden(chat, alice, true, start()).await,
        Ok(true)
    );
    assert_eq!(
        tx.set_conversation_hidden(chat, alice, true, later).await,
        Ok(false)
    );
    tx.commit().await.unwrap();

    assert_eq!(
        flags(&store, alice).await,
        [ConversationFlags {
            conversation_id: chat,
            hidden: true,
            peer_deleted: false,
            changed_at: start(),
        }]
    );
    assert!(flags(&store, bob).await.is_empty());

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.set_conversation_hidden(chat, alice, false, later).await,
        Ok(true)
    );
    tx.commit().await.unwrap();
    // The row stays, shown, with the time of the change.
    assert_eq!(
        flags(&store, alice).await,
        [ConversationFlags {
            conversation_id: chat,
            hidden: false,
            peer_deleted: false,
            changed_at: later,
        }]
    );
}

async fn unhiding_returns_the_members_who_had_hidden_it<B: Backend>(backend: &B) {
    let (store, chat, [alice, bob, _]) = direct(backend).await;
    let later = start() + TimeDelta::minutes(1);
    let mut tx = store.begin().await.unwrap();
    tx.set_conversation_hidden(chat, bob, true, start())
        .await
        .unwrap();
    assert_eq!(tx.unhide_conversation(chat, later).await, Ok(vec![bob]));
    assert_eq!(tx.unhide_conversation(chat, later).await, Ok(vec![]));
    tx.commit().await.unwrap();
    assert!(flags(&store, alice).await.is_empty());
    assert_eq!(
        flags(&store, bob).await,
        [ConversationFlags {
            conversation_id: chat,
            hidden: false,
            peer_deleted: false,
            changed_at: later,
        }]
    );
}

async fn a_deleted_peer_flags_the_direct_conversation<B: Backend>(backend: &B) {
    let (store, chat, [alice, bob, carol]) = direct(backend).await;
    let deleted_at = start() + TimeDelta::minutes(2);
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.mark_user_deleted(bob, deleted_at).await, Ok(true));
    // A repeat keeps the first time.
    assert_eq!(
        tx.mark_user_deleted(bob, deleted_at + TimeDelta::hours(1))
            .await,
        Ok(false)
    );
    tx.set_conversation_hidden(chat, alice, true, start())
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        flags(&store, alice).await,
        [ConversationFlags {
            conversation_id: chat,
            hidden: true,
            peer_deleted: true,
            changed_at: deleted_at,
        }]
    );
    // Bob's own view does not say his peer is deleted.
    assert!(flags(&store, bob).await.is_empty());
    assert!(flags(&store, carol).await.is_empty());

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.direct_peers(bob).await, Ok(vec![(chat, alice)]));
    assert_eq!(tx.direct_peers(carol).await, Ok(vec![]));
    tx.rollback().await.unwrap();
}

async fn groups_have_no_flags_and_no_direct_peers<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut group = conversation();
    group.kind = ConversationKind::Group;
    let id = group.id;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(group).await.unwrap();
    tx.insert_member(member(id, alice)).await.unwrap();
    tx.insert_member(member(id, bob)).await.unwrap();
    tx.mark_user_deleted(bob, start()).await.unwrap();
    assert_eq!(tx.direct_peers(alice).await, Ok(vec![]));
    tx.commit().await.unwrap();
    assert!(flags(&store, alice).await.is_empty());
}

async fn flags_need_a_member_and_a_known_user<B: Backend>(backend: &B) {
    let (store, chat, [_, _, carol]) = direct(backend).await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.set_conversation_hidden(chat, carol, true, start()).await,
        Err(DomainError::Store(_))
    ));
    drop(tx);
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.set_conversation_hidden(chat, carol, false, start())
            .await,
        Err(DomainError::Store(_))
    ));
    drop(tx);
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.mark_user_deleted(UserId::new(), start()).await,
        Err(DomainError::Store(_))
    ));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        hiding_is_per_user_and_reports_changes,
        unhiding_returns_the_members_who_had_hidden_it,
        a_deleted_peer_flags_the_direct_conversation,
        groups_have_no_flags_and_no_direct_peers,
        flags_need_a_member_and_a_known_user,
    ]
}
