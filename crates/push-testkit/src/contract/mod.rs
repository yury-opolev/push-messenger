//! The `Tx` contract (see `push_domain::store`), checked method by method.
//!
//! A method that fails may leave its transaction unusable (on Postgres, every later
//! statement fails), so every expected failure here ends its transaction.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use push_domain::{
    ClientMsgId, ContentType, Conversation, ConversationId, ConversationKind, Entry, EntryKind,
    InviteRecord, Member, MessageId, Role, Store, Tx, UserId,
};

use crate::{Backend, Case};

mod contacts;
mod conversations;
mod entries;
mod overviews;
mod positions;
mod transactions;
mod users;

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    let modules: Vec<(&str, Vec<Case<B>>)> = vec![
        ("contract::transactions", transactions::cases()),
        ("contract::users", users::cases()),
        ("contract::contacts", contacts::cases()),
        ("contract::conversations", conversations::cases()),
        ("contract::entries", entries::cases()),
        ("contract::positions", positions::cases()),
        ("contract::overviews", overviews::cases()),
    ];
    modules
        .into_iter()
        .flat_map(|(module, cases)| cases.into_iter().map(move |case| case.within(module)))
        .collect()
}

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

/// A direct conversation without a direct key, created by a user who need not exist.
fn conversation() -> Conversation {
    Conversation {
        id: ConversationId::new(),
        kind: ConversationKind::Direct,
        direct_key: None,
        title: None,
        created_by: UserId::new(),
        last_seq: 0,
        created_at: start(),
        updated_at: start(),
    }
}

/// A text message at `seq` from a sender who need not exist.
fn entry(conversation: ConversationId, seq: i64, client_msg_id: ClientMsgId) -> Entry {
    Entry {
        conversation_id: conversation,
        seq,
        message_id: MessageId::new(),
        client_msg_id,
        kind: EntryKind::Message,
        target_seq: None,
        sender_id: UserId::new(),
        sender_device_id: None,
        content_type: ContentType::Text,
        body: Some(b"hi".to_vec()),
        client_ts: None,
        server_ts: start(),
        edited_at: None,
        edit_seq: None,
        deleted_at: None,
    }
}

/// A current member who sees the conversation from its start.
fn member(conversation: ConversationId, user: UserId) -> Member {
    Member {
        conversation_id: conversation,
        user_id: user,
        role: Role::Member,
        visible_from_seq: 1,
        left_at_seq: None,
        read_seq: 0,
    }
}

fn invite(code: &str, user: UserId) -> InviteRecord {
    InviteRecord {
        code: code.into(),
        user_id: user,
        expires_at: start() + TimeDelta::days(1),
        max_uses: 1,
        uses: 0,
        revoked_at: None,
    }
}

fn seqs(entries: &[Entry]) -> Vec<i64> {
    entries.iter().map(|entry| entry.seq).collect()
}

/// A new store holding `conversation`.
async fn store_with<B: Backend>(backend: &B, conversation: Conversation) -> B::Store {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(conversation).await.unwrap();
    tx.commit().await.unwrap();
    store
}

/// Records these users.
async fn add_users<S: Store>(store: &S, users: &[UserId]) {
    let mut tx = store.begin().await.unwrap();
    for user in users {
        tx.insert_user(*user).await.unwrap();
    }
    tx.commit().await.unwrap();
}
