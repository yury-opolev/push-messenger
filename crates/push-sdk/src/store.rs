//! The local store (spec §8.3, §8.4): one SQLite database per signed-in user holding the
//! conversations, the messages (history), the receipt and read positions, the contacts
//! and the outbox of sends, edits and deletes not yet acknowledged.
//!
//! Every change that must survive a crash is one transaction: an outbox row and its local
//! echo are written together, before the first attempt, so a retry after a restart sends
//! the same `client_msg_id`. Positions only move forward (`MAX`), because receipts can
//! arrive in any order (spec §7.2).
//!
//! Opened with a [`DatabaseKey`], the whole file is encrypted (SQLite3 Multiple Ciphers):
//! the key is checked by the first read, before anything is written, so a wrong key never
//! damages the data.

use std::time::Duration;

use push_domain::{ClientMsgId, ConversationId, MAX_UNREAD, MessageId, UserId};
use push_proto::v1::{self, ContentType, Deliver, EntryKind};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::SdkError;
use crate::key::DatabaseKey;
use crate::model::{
    Contact, ContactState, Conversation, ConversationKind, Message, MessageKind, MessageStatus,
};

pub(crate) mod outbox;

/// The schema steps: `SCHEMA_STEPS[n]` brings a database from version `n` to `n + 1`.
/// Each runs in one transaction with the version bump, so a crash leaves the old version.
const SCHEMA_STEPS: &[&str] = &[SCHEMA_V1, SCHEMA_V2];

const SCHEMA_V1: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE conversation (
    id                 BLOB PRIMARY KEY,
    kind               INTEGER NOT NULL DEFAULT 0,
    peer_id            BLOB,
    title              TEXT,
    last_seq           INTEGER NOT NULL DEFAULT 0,
    -- The user's read position, including marks the server has not confirmed yet.
    read_seq           INTEGER NOT NULL DEFAULT 0,
    -- The highest read position the server confirmed.
    read_synced_seq    INTEGER NOT NULL DEFAULT 0,
    unread             INTEGER NOT NULL DEFAULT 0,
    peer_delivered_seq INTEGER NOT NULL DEFAULT 0,
    peer_read_seq      INTEGER NOT NULL DEFAULT 0,
    -- The highest seq this device stored from deliveries: what it acks.
    received_seq       INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE message (
    conversation_id BLOB NOT NULL,
    client_msg_id   BLOB NOT NULL,
    seq             INTEGER,
    message_id      BLOB,
    sender_id       BLOB NOT NULL,
    content_type    INTEGER NOT NULL,
    body            BLOB,
    client_ts       INTEGER,
    server_ts       INTEGER,
    edited_at       INTEGER,
    edit_seq        INTEGER NOT NULL DEFAULT 0,
    deleted         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (conversation_id, client_msg_id)
);
CREATE UNIQUE INDEX message_seq ON message (conversation_id, seq) WHERE seq IS NOT NULL;
CREATE TABLE contact (
    user_id BLOB PRIMARY KEY,
    state   INTEGER NOT NULL
);
CREATE TABLE outbox (
    client_msg_id        BLOB PRIMARY KEY,
    conversation_id      BLOB NOT NULL,
    kind                 INTEGER NOT NULL,
    target_message_id    BLOB,
    target_client_msg_id BLOB,
    content_type         INTEGER,
    body                 BLOB,
    -- 0 queued, 1 in flight.
    state                INTEGER NOT NULL DEFAULT 0,
    enqueued_at          INTEGER NOT NULL,
    attempts             INTEGER NOT NULL DEFAULT 0,
    -- Milliseconds since the Unix epoch.
    next_attempt_at      INTEGER NOT NULL DEFAULT 0,
    -- Send only after this row is acknowledged.
    parent_id            BLOB
);
CREATE INDEX outbox_order ON outbox (conversation_id);
";

/// Cards and answers: a card's `answer_seq` (the answer that closed it) and an answer's
/// `card_seq` (the card it answers); answers find their card by its server id.
const SCHEMA_V2: &str = "
ALTER TABLE message ADD COLUMN answer_seq INTEGER;
ALTER TABLE message ADD COLUMN card_seq INTEGER;
CREATE INDEX message_by_id ON message (conversation_id, message_id);
";

/// Refuses to go on with a key unless the linked SQLite is SQLite3 Multiple Ciphers: on a
/// plain SQLite `PRAGMA hexkey` is ignored and the data would be written in plaintext.
/// Probed on a scratch in-memory connection: asking the database itself would read its
/// header, which fails while an encrypted file has no key yet.
fn require_encryption() -> Result<(), SdkError> {
    let probe = Connection::open_in_memory()
        .and_then(|conn| conn.query_row("SELECT sqlite3mc_version()", [], |row| row.get(0)));
    check_encryption(probe)
}

fn check_encryption(probe: rusqlite::Result<String>) -> Result<(), SdkError> {
    probe.map(drop).map_err(|_| SdkError::EncryptionUnavailable)
}

/// Brings the database to the last version, one step at a time. Each step and its
/// version bump are one transaction: a failed or interrupted step changes nothing.
fn migrate(conn: &mut Connection, steps: &[&str]) -> Result<(), SdkError> {
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Read under the write lock: another opener may have migrated meanwhile.
        let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let step = usize::try_from(version).ok().filter(|v| *v <= steps.len());
        match step {
            Some(done) if done == steps.len() => return Ok(()),
            Some(done) => {
                tx.execute_batch(steps[done])?;
                tx.pragma_update(None, "user_version", version + 1)?;
                tx.commit()?;
            }
            None => {
                return Err(SdkError::Storage(format!(
                    "the database is from a newer SDK (schema {version})"
                )));
            }
        }
    }
}

/// What a delivered entry changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Applied {
    /// Stored already: a replay, or the same entry on a second connection.
    Duplicate,
    /// A message this device did not hold.
    Added(Message),
    /// One of this device's own messages, now stored by the server.
    Confirmed(Message),
    Edited(Message),
    Deleted(Message),
    /// An edit or delete of a message this device does not hold, or an older edit.
    Skipped,
}

/// The local database.
pub(crate) struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the database at `path`; `:memory:` keeps it in memory. With a
    /// key the file is encrypted; [`SdkError::WrongKey`] if the key does not open it (or a
    /// database that has one is opened without, or one without with a key), and
    /// [`SdkError::EncryptionUnavailable`] if the linked SQLite cannot encrypt.
    pub fn open(path: &str, key: Option<&DatabaseKey>) -> Result<Self, SdkError> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        if let Some(key) = key {
            require_encryption()?;
            let pragma = Zeroizing::new(format!("PRAGMA hexkey = '{}'", key.hex().as_str()));
            conn.execute_batch(&pragma)?;
        }
        // The first read of the file: it fails with "not a database" when the key does
        // not fit, before anything is written.
        if let Err(error) = conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        }) {
            return Err(match error.sqlite_error_code() {
                Some(rusqlite::ErrorCode::NotADatabase) => SdkError::WrongKey,
                _ => error.into(),
            });
        }
        // WAL: a crash never loses a committed outbox row; NORMAL syncs at checkpoints.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Deleted content is overwritten, not left in free space (see `wipe`).
        conn.pragma_update(None, "secure_delete", "ON")?;
        migrate(&mut conn, SCHEMA_STEPS)?;
        Ok(Self { conn })
    }

    /// Encrypts the database with `key`, re-encrypts it with a new one, or with `None`
    /// decrypts it. Every page is rewritten in one transaction: a crash leaves the old key
    /// or the new one, never a mix. The next [`Store::open`] needs the new key.
    ///
    /// After a crash during this call the app cannot know which key applies: it should
    /// try the other key when [`Store::open`] answers [`SdkError::WrongKey`].
    ///
    /// If the rekey succeeded but switching back to WAL did not, this returns `Ok`: the
    /// NEW key applies (the next open restores WAL). An `Err` means the key did not change.
    ///
    /// Rewrites the whole file while holding the connection: call it from
    /// `tokio::task::spawn_blocking`, never on an async worker thread.
    pub fn change_key(&mut self, key: Option<&DatabaseKey>) -> Result<(), SdkError> {
        require_encryption()?;
        // SQLite3 Multiple Ciphers changes keys only outside WAL mode.
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(SdkError::Storage(format!(
                "the key cannot change in journal mode {mode}"
            )));
        }
        let changed = match key {
            Some(key) => {
                let pragma = Zeroizing::new(format!("PRAGMA hexrekey = '{}'", key.hex().as_str()));
                self.conn.execute_batch(&pragma)
            }
            None => self.conn.execute_batch("PRAGMA rekey = ''"),
        };
        let restored = self.conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        });
        changed?;
        if !matches!(&restored, Ok(mode) if mode.eq_ignore_ascii_case("wal")) {
            tracing::warn!(
                ?restored,
                "the database key changed, but write-ahead logging was not restored"
            );
        }
        Ok(())
    }

    /// Makes every write fail, as a full disk would.
    #[cfg(test)]
    pub fn set_read_only(&self, read_only: bool) {
        self.conn
            .pragma_update(None, "query_only", read_only)
            .unwrap();
    }

    /// The user the database belongs to.
    pub fn owner(&self) -> Result<Option<UserId>, SdkError> {
        let value: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'owner'", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value.and_then(|value| Uuid::from_slice(&value).ok().map(UserId)))
    }

    /// Makes the database `user`'s. Another user's data is erased first, outbox
    /// included: whatever they had not sent is not this user's to send. True if it erased.
    pub fn bind_owner(&mut self, user: UserId) -> Result<bool, SdkError> {
        let owner = self.owner()?;
        if owner == Some(user) {
            return Ok(false);
        }
        self.wipe(Some(user))?;
        Ok(owner.is_some())
    }

    /// Erases everything: the user signed this device out. Whole-file work behind the
    /// connection: call it from `tokio::task::spawn_blocking`, not on an async worker.
    pub fn erase_all(&mut self) -> Result<(), SdkError> {
        self.wipe(None)
    }

    /// Deletes every row, then records `owner`, and leaves nothing of the old rows in the
    /// files: `secure_delete` overwrites deleted content, and the write-ahead log, which
    /// still holds the old pages, is checkpointed and truncated.
    fn wipe(&mut self, owner: Option<UserId>) -> Result<(), SdkError> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM outbox; DELETE FROM message; DELETE FROM conversation; DELETE FROM contact; DELETE FROM meta;",
        )?;
        if let Some(owner) = owner {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('owner', ?1)",
                params![bytes(owner.0)],
            )?;
        }
        tx.commit()?;
        // (busy, log frames, checkpointed): busy = 1 means a reader kept the log.
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            return Err(SdkError::Storage(
                "the old data may remain in the write-ahead log: a reader is busy".to_owned(),
            ));
        }
        Ok(())
    }

    fn me(&self) -> Result<UserId, SdkError> {
        self.owner()?.ok_or(SdkError::NotSignedIn)
    }

    // ----- conversations -----

    pub fn conversation(&self, id: ConversationId) -> Result<Option<Conversation>, SdkError> {
        Ok(self
            .conn
            .query_row(
                &format!("{CONVERSATION_COLUMNS} WHERE id = ?1"),
                params![bytes(id.0)],
                conversation_row,
            )
            .optional()?)
    }

    /// Every conversation, most recently changed first.
    pub fn conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        let mut statement = self.conn.prepare(&format!(
            "{CONVERSATION_COLUMNS} ORDER BY updated_at DESC, id"
        ))?;
        let rows = statement.query_map([], conversation_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a direct conversation the user opened.
    pub fn open_direct(
        &mut self,
        id: ConversationId,
        peer: UserId,
    ) -> Result<Conversation, SdkError> {
        self.conn.execute(
            "INSERT INTO conversation (id, kind, peer_id) VALUES (?1, 1, ?2)
             ON CONFLICT (id) DO UPDATE SET kind = 1, peer_id = excluded.peer_id",
            params![bytes(id.0), bytes(peer.0)],
        )?;
        self.conversation(id)?
            .ok_or_else(|| SdkError::Storage("the conversation vanished".to_owned()))
    }

    /// Takes a conversation from a listing: positions only move forward, the unread count
    /// is the server's. The conversation if it changed.
    pub fn apply_summary(
        &mut self,
        summary: &v1::ConversationSummary,
    ) -> Result<Option<Conversation>, SdkError> {
        let me = self.me()?;
        let id = ConversationId(id_of(&summary.conversation_id)?);
        let before = self.conversation(id)?;
        let peer = summary
            .members
            .iter()
            .filter_map(|member| Uuid::from_slice(member).ok())
            .find(|member| *member != me.0);
        let (delivered, read) = match &summary.receipts {
            Some(v1::conversation_summary::Receipts::Direct(direct)) => {
                (direct.peer_delivered_seq, direct.peer_read_seq)
            }
            Some(v1::conversation_summary::Receipts::Group(group)) => {
                (group.all_delivered_seq, group.all_read_seq)
            }
            None => (0, 0),
        };
        let kind = ConversationKind::from_wire(summary.kind());
        self.conn.execute(
            "INSERT INTO conversation (id, kind, peer_id, title, last_seq, read_seq, read_synced_seq,
                 unread, peer_delivered_seq, peer_read_seq, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (id) DO UPDATE SET
                 kind = excluded.kind,
                 peer_id = COALESCE(excluded.peer_id, peer_id),
                 title = excluded.title,
                 last_seq = MAX(last_seq, excluded.last_seq),
                 read_seq = MAX(read_seq, excluded.read_seq),
                 read_synced_seq = MAX(read_synced_seq, excluded.read_synced_seq),
                 unread = CASE WHEN excluded.read_seq >= read_seq THEN excluded.unread ELSE unread END,
                 peer_delivered_seq = MAX(peer_delivered_seq, excluded.peer_delivered_seq),
                 peer_read_seq = MAX(peer_read_seq, excluded.peer_read_seq),
                 updated_at = MAX(updated_at, excluded.updated_at)",
            params![
                bytes(id.0),
                kind.code(),
                peer.map(bytes),
                summary.title,
                summary.last_seq,
                summary.read_seq,
                i64::from(summary.unread.min(MAX_UNREAD)),
                delivered,
                read,
                summary.updated_at,
            ],
        )?;
        let after = self.conversation(id)?;
        Ok(if after == before { None } else { after })
    }

    /// A receipt: the highest positions win. The conversation if they moved.
    pub fn apply_receipt(
        &mut self,
        id: ConversationId,
        delivered_seq: i64,
        read_seq: i64,
    ) -> Result<Option<Conversation>, SdkError> {
        let changed = self.conn.execute(
            "UPDATE conversation SET
                 peer_delivered_seq = MAX(peer_delivered_seq, ?2, ?3),
                 peer_read_seq = MAX(peer_read_seq, ?3)
             WHERE id = ?1 AND (peer_delivered_seq < MAX(?2, ?3) OR peer_read_seq < ?3)",
            params![bytes(id.0), delivered_seq, read_seq],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.conversation(id)
    }

    /// The user's read position as the server confirmed it (`ReadPosition`, or after a
    /// `ReadUpTo`). The conversation if it moved.
    pub fn apply_read_position(
        &mut self,
        id: ConversationId,
        read_seq: i64,
    ) -> Result<Option<Conversation>, SdkError> {
        let before = self.conversation(id)?;
        self.conn.execute(
            "UPDATE conversation SET read_synced_seq = MAX(read_synced_seq, ?2),
                 read_seq = MAX(read_seq, ?2) WHERE id = ?1",
            params![bytes(id.0), read_seq],
        )?;
        self.recount_unread(id)?;
        let after = self.conversation(id)?;
        Ok(if after == before { None } else { after })
    }

    /// The user read up to `seq`: moves the local position forward at once, to be sent
    /// as `ReadUpTo`, but never past the last `seq` the device knows. The conversation if
    /// it moved.
    pub fn mark_read(
        &mut self,
        id: ConversationId,
        seq: i64,
    ) -> Result<Option<Conversation>, SdkError> {
        let changed = self.conn.execute(
            "UPDATE conversation SET read_seq = MIN(?2, last_seq)
             WHERE id = ?1 AND read_seq < MIN(?2, last_seq)",
            params![bytes(id.0), seq],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.recount_unread(id)?;
        self.conversation(id)
    }

    /// Read positions not confirmed by the server yet: `(conversation, read_seq)`.
    pub fn unsynced_reads(&self) -> Result<Vec<(ConversationId, i64)>, SdkError> {
        let mut statement = self
            .conn
            .prepare("SELECT id, read_seq FROM conversation WHERE read_seq > read_synced_seq")?;
        let rows = statement.query_map([], |row| Ok((uuid_at(row, 0)?, row.get(1)?)))?;
        Ok(rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(id, seq)| (ConversationId(id), seq))
            .collect())
    }

    /// The unread count from the messages held: others' live messages after the read
    /// position, at most 100.
    fn recount_unread(&mut self, id: ConversationId) -> Result<(), SdkError> {
        let me = self.me()?;
        self.conn.execute(
            "UPDATE conversation SET unread = (
                 SELECT MIN(COUNT(*), ?3) FROM message m
                 WHERE m.conversation_id = conversation.id AND m.seq > conversation.read_seq
                   AND m.sender_id <> ?2 AND m.deleted = 0 AND m.content_type IN (1, 3, 4))
             WHERE id = ?1",
            params![bytes(id.0), bytes(me.0), i64::from(MAX_UNREAD)],
        )?;
        Ok(())
    }

    /// Per conversation, the highest `seq` stored from deliveries.
    pub fn received(&self, id: ConversationId) -> Result<i64, SdkError> {
        Ok(self
            .conn
            .query_row(
                "SELECT received_seq FROM conversation WHERE id = ?1",
                params![bytes(id.0)],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    // ----- messages -----

    /// Applies a delivered entry (spec §4.3, §4.6): new messages are stored, an edit or a
    /// delete changes its target only if it is newer than the target's last change, and
    /// entries at or below the conversation's received position are replays. Returns what
    /// changed and the conversation when it was new or its summary changed.
    pub fn apply_deliver(
        &mut self,
        entry: &Deliver,
    ) -> Result<(Applied, Option<Conversation>, bool), SdkError> {
        let me = self.me()?;
        let id = ConversationId(id_of(&entry.conversation_id)?);
        let before = self.conversation(id)?;
        let created = before.is_none();
        if let Some(conversation) = &before {
            if entry.seq <= self.received(conversation.id)? {
                return Ok((Applied::Duplicate, None, false));
            }
        } else {
            self.conn.execute(
                "INSERT INTO conversation (id) VALUES (?1)",
                params![bytes(id.0)],
            )?;
        }
        let applied = match entry.kind() {
            EntryKind::Message | EntryKind::System => self.store_entry(entry)?,
            EntryKind::Edit => self.apply_edit_entry(id, entry)?,
            EntryKind::Delete => self.apply_delete_entry(id, entry)?,
            EntryKind::Unspecified => Applied::Skipped,
        };
        self.conn.execute(
            "UPDATE conversation SET received_seq = MAX(received_seq, ?2),
                 last_seq = MAX(last_seq, ?2), updated_at = MAX(updated_at, ?3)
             WHERE id = ?1",
            params![bytes(id.0), entry.seq, entry.server_ts],
        )?;
        let counts = matches!(&applied, Applied::Added(message)
            if message.sender_id != me && message.kind != MessageKind::System && !message.deleted);
        if counts || matches!(applied, Applied::Deleted(_)) {
            self.recount_unread(id)?;
        }
        let after = self.conversation(id)?;
        let changed = if after == before { None } else { after };
        Ok((applied, changed, created))
    }

    /// A message or system entry, from a delivery or a history page: the device's own
    /// pending message is confirmed; one held already is brought to its current state.
    fn store_entry(&mut self, entry: &Deliver) -> Result<Applied, SdkError> {
        let id = ConversationId(id_of(&entry.conversation_id)?);
        let client_msg_id = ClientMsgId(id_of(&entry.client_msg_id)?);
        let existing = self.message(id, client_msg_id)?;
        let body = entry.body.as_deref();
        match existing {
            Some(held) => {
                // The position of the entry is always taken; the content only if the
                // message is not deleted and the entry is not older than what is held:
                // a stale page never brings a body back or replaces a newer edit.
                self.conn.execute(
                    "UPDATE message SET seq = ?3, message_id = ?4, server_ts = ?5,
                         answer_seq = COALESCE(answer_seq, ?6), card_seq = COALESCE(card_seq, ?7)
                     WHERE conversation_id = ?1 AND client_msg_id = ?2",
                    params![
                        bytes(id.0),
                        bytes(client_msg_id.0),
                        entry.seq,
                        entry.message_id,
                        entry.server_ts,
                        entry.answer_seq,
                        card_seq(entry),
                    ],
                )?;
                self.conn.execute(
                    "UPDATE message SET body = ?3, edited_at = ?4, edit_seq = MAX(edit_seq, ?5),
                         deleted = ?6
                     WHERE conversation_id = ?1 AND client_msg_id = ?2 AND deleted = 0
                         AND (?6 = 1 OR ?5 >= edit_seq)",
                    params![
                        bytes(id.0),
                        bytes(client_msg_id.0),
                        if entry.deleted { None } else { body },
                        entry.edited_at,
                        entry.edit_seq.unwrap_or(0),
                        entry.deleted,
                    ],
                )?;
                let now = self.message(id, client_msg_id)?.expect("just updated");
                Ok(if held.seq.is_none() {
                    Applied::Confirmed(now)
                } else {
                    Applied::Duplicate
                })
            }
            None => {
                let inserted = self.conn.execute(
                    "INSERT OR IGNORE INTO message (conversation_id, client_msg_id, seq, message_id,
                         sender_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, deleted,
                         answer_seq, card_seq)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        bytes(id.0),
                        bytes(client_msg_id.0),
                        entry.seq,
                        entry.message_id,
                        entry.sender_id,
                        content_type_code(entry),
                        if entry.deleted { None } else { body },
                        entry.client_ts,
                        entry.server_ts,
                        entry.edited_at,
                        entry.edit_seq.unwrap_or(0),
                        entry.deleted,
                        entry.answer_seq,
                        card_seq(entry),
                    ],
                )?;
                if inserted == 0 {
                    return Ok(Applied::Duplicate);
                }
                Ok(Applied::Added(
                    self.message(id, client_msg_id)?.expect("just inserted"),
                ))
            }
        }
    }

    fn target_at(
        &self,
        id: ConversationId,
        seq: Option<i64>,
    ) -> Result<Option<(ClientMsgId, i64)>, SdkError> {
        let Some(seq) = seq else {
            return Ok(None);
        };
        Ok(self
            .conn
            .query_row(
                "SELECT client_msg_id, edit_seq FROM message WHERE conversation_id = ?1 AND seq = ?2",
                params![bytes(id.0), seq],
                |row| Ok((uuid_at(row, 0)?, row.get(1)?)),
            )
            .optional()?
            .map(|(client_msg_id, edit_seq)| (ClientMsgId(client_msg_id), edit_seq)))
    }

    fn apply_edit_entry(
        &mut self,
        id: ConversationId,
        entry: &Deliver,
    ) -> Result<Applied, SdkError> {
        let Some((target, edit_seq)) = self.target_at(id, entry.target_seq)? else {
            return Ok(Applied::Skipped);
        };
        // A superseded edit carries no body; the newer one follows.
        let Some(body) = entry.body.as_deref() else {
            return Ok(Applied::Skipped);
        };
        if entry.seq <= edit_seq {
            return Ok(Applied::Skipped);
        }
        self.edit(id, target, body, entry.server_ts, entry.seq)
    }

    fn apply_delete_entry(
        &mut self,
        id: ConversationId,
        entry: &Deliver,
    ) -> Result<Applied, SdkError> {
        let Some((target, edit_seq)) = self.target_at(id, entry.target_seq)? else {
            return Ok(Applied::Skipped);
        };
        if entry.seq <= edit_seq {
            return Ok(Applied::Skipped);
        }
        self.erase(id, target, entry.seq)
    }

    /// Gives a message a new body, from an edit entry at `seq`.
    pub fn edit(
        &mut self,
        id: ConversationId,
        target: ClientMsgId,
        body: &[u8],
        edited_at: i64,
        seq: i64,
    ) -> Result<Applied, SdkError> {
        let changed = self.conn.execute(
            "UPDATE message SET body = ?3, edited_at = ?4, edit_seq = ?5
             WHERE conversation_id = ?1 AND client_msg_id = ?2 AND deleted = 0 AND edit_seq < ?5",
            params![bytes(id.0), bytes(target.0), body, edited_at, seq],
        )?;
        if changed == 0 {
            return Ok(Applied::Skipped);
        }
        Ok(Applied::Edited(
            self.message(id, target)?.expect("just edited"),
        ))
    }

    /// Erases a message's body, from a delete entry at `seq`.
    pub fn erase(
        &mut self,
        id: ConversationId,
        target: ClientMsgId,
        seq: i64,
    ) -> Result<Applied, SdkError> {
        let changed = self.conn.execute(
            "UPDATE message SET body = NULL, deleted = 1, edit_seq = MAX(edit_seq, ?3)
             WHERE conversation_id = ?1 AND client_msg_id = ?2 AND deleted = 0",
            params![bytes(id.0), bytes(target.0), seq],
        )?;
        if changed == 0 {
            return Ok(Applied::Skipped);
        }
        self.recount_unread(id)?;
        Ok(Applied::Deleted(
            self.message(id, target)?.expect("just erased"),
        ))
    }

    /// Stores a history page: messages and system entries in their current state. The
    /// received position is not moved: history is older than what deliveries bring.
    pub fn store_history(&mut self, entries: &[Deliver]) -> Result<Vec<Message>, SdkError> {
        let mut stored = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = ConversationId(id_of(&entry.conversation_id)?);
            if self.conversation(id)?.is_none() {
                self.conn.execute(
                    "INSERT INTO conversation (id) VALUES (?1)",
                    params![bytes(id.0)],
                )?;
            }
            self.store_entry(entry)?;
            let client_msg_id = ClientMsgId(id_of(&entry.client_msg_id)?);
            if let Some(message) = self.message(id, client_msg_id)? {
                self.mark_answered(&message)?;
                stored.push(message);
            }
        }
        // An answer later in the page may have closed a card earlier in it.
        for message in &mut stored {
            if message.kind == MessageKind::Rich && message.answer_seq.is_none() {
                if let Some(now) = self.message(message.conversation_id, message.client_msg_id)? {
                    *message = now;
                }
            }
        }
        Ok(stored)
    }

    /// An answer stored by the server closes its card: the card's `answer_seq` is set
    /// unless it was answered already. The card if it changed.
    pub fn mark_answered(&mut self, answer: &Message) -> Result<Option<Message>, SdkError> {
        let (MessageKind::Answer, Some(seq), Some(card_seq)) =
            (answer.kind, answer.seq, answer.card_seq)
        else {
            return Ok(None);
        };
        let changed = self.conn.execute(
            "UPDATE message SET answer_seq = ?3
             WHERE conversation_id = ?1 AND seq = ?2 AND content_type = 3 AND answer_seq IS NULL",
            params![bytes(answer.conversation_id.0), card_seq, seq],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.message_at(answer.conversation_id, card_seq)
    }

    /// The message at `seq`, if held.
    pub fn message_at(&self, id: ConversationId, seq: i64) -> Result<Option<Message>, SdkError> {
        let me = self.me()?;
        Ok(self
            .conn
            .query_row(
                &format!("{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.seq = ?2"),
                params![bytes(id.0), seq],
                |row| message_row(row, me),
            )
            .optional()?)
    }

    /// The message the server stored as `message_id`, if held.
    pub fn message_by_id(
        &self,
        id: ConversationId,
        message_id: MessageId,
    ) -> Result<Option<Message>, SdkError> {
        let me = self.me()?;
        Ok(self
            .conn
            .query_row(
                &format!("{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.message_id = ?2"),
                params![bytes(id.0), bytes(message_id.0)],
                |row| message_row(row, me),
            )
            .optional()?)
    }

    pub fn message(
        &self,
        id: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> Result<Option<Message>, SdkError> {
        let me = self.me()?;
        Ok(self
            .conn
            .query_row(
                &format!("{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.client_msg_id = ?2"),
                params![bytes(id.0), bytes(client_msg_id.0)],
                |row| message_row(row, me),
            )
            .optional()?)
    }

    /// Up to `limit` stored messages before `before_seq`, oldest first; without
    /// `before_seq`, the newest ones followed by the messages still in the outbox.
    pub fn messages(
        &self,
        id: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        let me = self.me()?;
        let mut statement = self.conn.prepare(&format!(
            "{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.seq IS NOT NULL AND m.seq < ?2
             ORDER BY m.seq DESC LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![bytes(id.0), before_seq.unwrap_or(i64::MAX), limit],
            |row| message_row(row, me),
        )?;
        let mut messages: Vec<Message> = rows.collect::<Result<_, _>>()?;
        messages.reverse();
        if before_seq.is_none() {
            let mut statement = self.conn.prepare(&format!(
                "{MESSAGE_COLUMNS} WHERE m.conversation_id = ?1 AND m.seq IS NULL ORDER BY m.rowid"
            ))?;
            let pending = statement.query_map(params![bytes(id.0)], |row| message_row(row, me))?;
            messages.extend(pending.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(messages)
    }

    // ----- contacts -----

    pub fn contacts(&self) -> Result<Vec<Contact>, SdkError> {
        let mut statement = self
            .conn
            .prepare("SELECT user_id, state FROM contact WHERE state <> 0 ORDER BY user_id")?;
        let rows = statement.query_map([], |row| {
            Ok(Contact {
                user_id: UserId(uuid_at(row, 0)?),
                state: ContactState::from_code(row.get(1)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a relationship; true if it changed.
    pub fn set_contact(&mut self, contact: Contact) -> Result<bool, SdkError> {
        let changed = self.conn.execute(
            "INSERT INTO contact (user_id, state) VALUES (?1, ?2)
             ON CONFLICT (user_id) DO UPDATE SET state = excluded.state WHERE state <> excluded.state",
            params![bytes(contact.user_id.0), contact.state.code()],
        )?;
        Ok(changed > 0)
    }

    /// Replaces every relationship with a listing; the ones that changed.
    pub fn replace_contacts(&mut self, listed: &[Contact]) -> Result<Vec<Contact>, SdkError> {
        let mut changed = Vec::new();
        for contact in self.contacts()? {
            if !listed.iter().any(|other| other.user_id == contact.user_id) {
                let none = Contact {
                    user_id: contact.user_id,
                    state: ContactState::None,
                };
                if self.set_contact(none)? {
                    changed.push(none);
                }
            }
        }
        for contact in listed {
            if self.set_contact(*contact)? {
                changed.push(*contact);
            }
        }
        Ok(changed)
    }
}

const CONVERSATION_COLUMNS: &str = "SELECT id, kind, peer_id, title, last_seq, read_seq, unread,
    peer_delivered_seq, peer_read_seq, updated_at FROM conversation";

fn conversation_row(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    let peer: Option<Vec<u8>> = row.get(2)?;
    let unread: i64 = row.get(6)?;
    Ok(Conversation {
        id: ConversationId(uuid_at(row, 0)?),
        kind: ConversationKind::from_code(row.get(1)?),
        peer: peer.and_then(|peer| Uuid::from_slice(&peer).ok().map(UserId)),
        title: row.get(3)?,
        last_seq: row.get(4)?,
        read_seq: row.get(5)?,
        unread: u32::try_from(unread).unwrap_or(0),
        peer_delivered_seq: row.get(7)?,
        peer_read_seq: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

const MESSAGE_COLUMNS: &str = "SELECT m.conversation_id, m.client_msg_id, m.message_id, m.seq,
    m.sender_id, m.content_type, m.body, m.client_ts, m.server_ts, m.edited_at, m.deleted,
    c.peer_delivered_seq, c.peer_read_seq, m.answer_seq, m.card_seq
    FROM message m JOIN conversation c ON c.id = m.conversation_id";

fn message_row(row: &Row<'_>, me: UserId) -> rusqlite::Result<Message> {
    let message_id: Option<Vec<u8>> = row.get(2)?;
    let seq: Option<i64> = row.get(3)?;
    let sender_id = UserId(uuid_at(row, 4)?);
    let content_type: i64 = row.get(5)?;
    let body: Option<Vec<u8>> = row.get(6)?;
    let (delivered, read): (i64, i64) = (row.get(11)?, row.get(12)?);
    let status = match seq {
        _ if sender_id != me => MessageStatus::Received,
        None => MessageStatus::Pending,
        Some(seq) if seq <= read => MessageStatus::Read,
        Some(seq) if seq <= delivered => MessageStatus::Delivered,
        Some(_) => MessageStatus::Sent,
    };
    Ok(Message {
        conversation_id: ConversationId(uuid_at(row, 0)?),
        client_msg_id: ClientMsgId(uuid_at(row, 1)?),
        message_id: message_id.and_then(|id| Uuid::from_slice(&id).ok().map(MessageId)),
        seq,
        sender_id,
        kind: MessageKind::from_code(content_type),
        body: body.map(|body| String::from_utf8_lossy(&body).into_owned()),
        client_ts: row.get(7)?,
        server_ts: row.get(8)?,
        edited_at: row.get(9)?,
        deleted: row.get(10)?,
        status,
        answer_seq: row.get(13)?,
        card_seq: row.get(14)?,
    })
}

fn uuid_at(row: &Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    let value: Vec<u8> = row.get(index)?;
    Uuid::from_slice(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(error),
        )
    })
}

fn content_type_code(entry: &Deliver) -> i64 {
    match entry.content_type() {
        ContentType::SystemEvent => MessageKind::System,
        ContentType::Rich => MessageKind::Rich,
        ContentType::Answer => MessageKind::Answer,
        ContentType::Text | ContentType::Unspecified => MessageKind::Text,
    }
    .code()
}

/// An answer's card: the entry's `target_seq`. Nothing for any other entry.
fn card_seq(entry: &Deliver) -> Option<i64> {
    (entry.content_type() == ContentType::Answer)
        .then_some(entry.target_seq)
        .flatten()
}

fn id_of(value: &[u8]) -> Result<Uuid, SdkError> {
    Uuid::from_slice(value)
        .map_err(|_| SdkError::Network("the server sent an id that is not 16 bytes".to_owned()))
}

pub(crate) fn bytes(id: Uuid) -> Vec<u8> {
    id.as_bytes().to_vec()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct Fixture {
        pub store: Store,
        pub me: UserId,
        pub peer: UserId,
        pub conversation: ConversationId,
    }

    pub(crate) fn fixture() -> Fixture {
        let mut store = Store::open(":memory:", None).unwrap();
        let (me, peer) = (UserId::new(), UserId::new());
        store.bind_owner(me).unwrap();
        let conversation = ConversationId::new();
        store.open_direct(conversation, peer).unwrap();
        Fixture {
            store,
            me,
            peer,
            conversation,
        }
    }

    pub(crate) fn entry(
        conversation: ConversationId,
        sender: UserId,
        seq: i64,
        kind: EntryKind,
    ) -> Deliver {
        Deliver {
            conversation_id: bytes(conversation.0),
            seq,
            message_id: bytes(Uuid::now_v7()),
            client_msg_id: bytes(Uuid::now_v7()),
            kind: kind.into(),
            target_seq: None,
            sender_id: bytes(sender.0),
            content_type: ContentType::Text.into(),
            body: Some(format!("message {seq}").into_bytes()),
            client_ts: None,
            server_ts: 1_000 + seq,
            edited_at: None,
            edit_seq: None,
            deleted: false,
            answer_seq: None,
        }
    }

    #[test]
    fn a_delivered_message_is_stored_once_and_counts_as_unread() {
        let mut f = fixture();
        let first = entry(f.conversation, f.peer, 1, EntryKind::Message);
        let (applied, conversation, created) = f.store.apply_deliver(&first).unwrap();
        assert!(
            matches!(applied, Applied::Added(ref message) if message.body.as_deref() == Some("message 1"))
        );
        assert!(!created);
        let conversation = conversation.unwrap();
        assert_eq!((conversation.last_seq, conversation.unread), (1, 1));
        assert_eq!(f.store.received(f.conversation).unwrap(), 1);
        // The same entry again (a second connection, a replay before the ack) changes nothing.
        let (again, changed, _) = f.store.apply_deliver(&first).unwrap();
        assert_eq!((again, changed), (Applied::Duplicate, None));
        assert_eq!(f.store.messages(f.conversation, None, 50).unwrap().len(), 1);
    }

    #[test]
    fn an_edit_or_delete_applies_only_if_newer_and_only_to_a_held_message() {
        let mut f = fixture();
        f.store
            .apply_deliver(&entry(f.conversation, f.peer, 1, EntryKind::Message))
            .unwrap();
        let mut delete = entry(f.conversation, f.peer, 3, EntryKind::Delete);
        delete.target_seq = Some(1);
        delete.body = None;
        let (applied, _, _) = f.store.apply_deliver(&delete).unwrap();
        assert!(
            matches!(applied, Applied::Deleted(ref message) if message.deleted && message.body.is_none())
        );
        // An older edit arriving later (seq 2 < 3) cannot bring the body back... it is a
        // replay below the received position anyway, but even a held target refuses it.
        let applied = f
            .store
            .edit(
                f.conversation,
                message_at(&f, 1).client_msg_id,
                b"back",
                5,
                2,
            )
            .unwrap();
        assert_eq!(applied, Applied::Skipped);
        assert_eq!(message_at(&f, 1).body, None);

        let mut orphan = entry(f.conversation, f.peer, 4, EntryKind::Edit);
        orphan.target_seq = Some(99);
        assert_eq!(f.store.apply_deliver(&orphan).unwrap().0, Applied::Skipped);
        assert_eq!(f.store.received(f.conversation).unwrap(), 4);
    }

    #[test]
    fn a_stale_history_page_never_brings_back_a_deleted_body() {
        let mut f = fixture();
        let original = entry(f.conversation, f.peer, 1, EntryKind::Message);
        f.store.apply_deliver(&original).unwrap();
        let mut delete = entry(f.conversation, f.peer, 3, EntryKind::Delete);
        delete.target_seq = Some(1);
        delete.body = None;
        f.store.apply_deliver(&delete).unwrap();

        f.store.store_history(&[original]).unwrap();
        let held = message_at(&f, 1);
        assert!(held.deleted);
        assert_eq!(held.body, None);
    }

    #[test]
    fn a_stale_history_page_never_replaces_a_newer_edit() {
        let mut f = fixture();
        let original = entry(f.conversation, f.peer, 1, EntryKind::Message);
        f.store.apply_deliver(&original).unwrap();
        let mut edit = entry(f.conversation, f.peer, 2, EntryKind::Edit);
        edit.target_seq = Some(1);
        edit.body = Some(b"newer".to_vec());
        f.store.apply_deliver(&edit).unwrap();
        assert_eq!(message_at(&f, 1).body.as_deref(), Some("newer"));

        f.store
            .store_history(std::slice::from_ref(&original))
            .unwrap();
        assert_eq!(message_at(&f, 1).body.as_deref(), Some("newer"));

        let mut current = original;
        current.body = Some(b"newest".to_vec());
        current.edit_seq = Some(5);
        f.store.store_history(&[current]).unwrap();
        assert_eq!(message_at(&f, 1).body.as_deref(), Some("newest"));
    }

    fn message_at(f: &Fixture, seq: i64) -> Message {
        f.store
            .messages(f.conversation, None, 100)
            .unwrap()
            .into_iter()
            .find(|message| message.seq == Some(seq))
            .unwrap()
    }

    #[test]
    fn reading_moves_forward_only_and_recounts_unread() {
        let mut f = fixture();
        for seq in 1..=3 {
            f.store
                .apply_deliver(&entry(f.conversation, f.peer, seq, EntryKind::Message))
                .unwrap();
        }
        let read = f.store.mark_read(f.conversation, 2).unwrap().unwrap();
        assert_eq!((read.read_seq, read.unread), (2, 1));
        assert!(f.store.mark_read(f.conversation, 1).unwrap().is_none());
        assert_eq!(f.store.unsynced_reads().unwrap(), [(f.conversation, 2)]);
        f.store.apply_read_position(f.conversation, 2).unwrap();
        assert!(f.store.unsynced_reads().unwrap().is_empty());
        // Not past the last message: a read position beyond it would hide the next ones.
        let read = f.store.mark_read(f.conversation, 1_000).unwrap().unwrap();
        assert_eq!((read.read_seq, read.unread), (3, 0));
    }

    /// A database file in a directory of its own next to the test binary, removed when
    /// the value is dropped.
    pub(crate) struct Scratch {
        dir: std::path::PathBuf,
    }

    impl Scratch {
        pub fn new() -> Self {
            let exe = std::env::current_exe().unwrap();
            let dir = exe
                .parent()
                .unwrap()
                .join("push-sdk-test-data")
                .join(Uuid::now_v7().to_string());
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        pub fn database(&self) -> String {
            self.dir.join("push.db").to_string_lossy().into_owned()
        }

        /// Whether the database or its write-ahead log holds `marker`.
        pub fn holds(&self, marker: &str) -> bool {
            ["push.db", "push.db-wal"].iter().any(|name| {
                std::fs::read(self.dir.join(name)).is_ok_and(|bytes| {
                    bytes
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes())
                })
            })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn erasing_leaves_no_trace_in_the_files() {
        let scratch = Scratch::new();
        let mut store = Store::open(&scratch.database(), None).unwrap();
        let (me, peer, conversation) = (UserId::new(), UserId::new(), ConversationId::new());
        store.bind_owner(me).unwrap();
        store.open_direct(conversation, peer).unwrap();
        let marker = format!("secret-{}", Uuid::now_v7());
        for seq in 1..=20 {
            let mut delivered = entry(conversation, peer, seq, EntryKind::Message);
            delivered.body = Some(format!("{marker} {seq}").into_bytes());
            store.apply_deliver(&delivered).unwrap();
        }
        assert!(scratch.holds(&marker), "a plain database holds the text");

        store.erase_all().unwrap();
        assert!(!scratch.holds(&marker), "erased text is still in the files");
        assert!(store.conversations().unwrap().is_empty());

        // Another user's sign-in erases the same way.
        store.bind_owner(me).unwrap();
        store.open_direct(conversation, peer).unwrap();
        let mut delivered = entry(conversation, peer, 1, EntryKind::Message);
        delivered.body = Some(marker.clone().into_bytes());
        store.apply_deliver(&delivered).unwrap();
        assert!(scratch.holds(&marker));
        assert!(store.bind_owner(UserId::new()).unwrap());
        assert!(!scratch.holds(&marker));
    }

    /// Fills `store` for `me` with a delivered message and a queued one, both carrying
    /// `marker`; the conversation.
    fn fill(store: &mut Store, me: UserId, marker: &str) -> ConversationId {
        let (peer, conversation) = (UserId::new(), ConversationId::new());
        store.bind_owner(me).unwrap();
        store.open_direct(conversation, peer).unwrap();
        let mut delivered = entry(conversation, peer, 1, EntryKind::Message);
        delivered.body = Some(format!("{marker} received").into_bytes());
        store.apply_deliver(&delivered).unwrap();
        store
            .queue_send(
                conversation,
                ClientMsgId::new(),
                &format!("{marker} queued"),
                1,
            )
            .unwrap();
        conversation
    }

    fn key(byte: u8) -> DatabaseKey {
        DatabaseKey::from_bytes(&[byte; DatabaseKey::LEN]).unwrap()
    }

    #[test]
    fn with_a_key_the_files_hold_no_plaintext() {
        let scratch = Scratch::new();
        let marker = format!("secret-{}", Uuid::now_v7());
        let me = UserId::new();
        let mut store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
        let conversation = fill(&mut store, me, &marker);
        assert!(!scratch.holds(&marker), "the open database and its log");
        drop(store);
        assert!(!scratch.holds(&marker), "the closed database");
        assert!(
            !scratch.holds("SQLite format 3"),
            "even the header is encrypted"
        );

        let store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
        assert_eq!(store.messages(conversation, None, 10).unwrap().len(), 2);
    }

    #[test]
    fn a_wrong_or_missing_key_is_refused_and_changes_nothing() {
        let scratch = Scratch::new();
        let me = UserId::new();
        let mut store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
        let conversation = fill(&mut store, me, "kept");
        drop(store);
        for wrong in [Some(key(2)), None] {
            assert_eq!(
                Store::open(&scratch.database(), wrong.as_ref()).err(),
                Some(SdkError::WrongKey)
            );
        }
        let store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
        assert_eq!(store.owner().unwrap(), Some(me));
        assert_eq!(store.messages(conversation, None, 10).unwrap().len(), 2);
        assert_eq!(store.outbox().unwrap().len(), 1);
    }

    #[test]
    fn a_key_can_be_added_changed_and_removed() {
        let scratch = Scratch::new();
        let marker = format!("secret-{}", Uuid::now_v7());
        let me = UserId::new();
        let mut store = Store::open(&scratch.database(), None).unwrap();
        let conversation = fill(&mut store, me, &marker);
        store.change_key(Some(&key(1))).unwrap();
        assert!(!scratch.holds(&marker), "encrypted in place");
        drop(store);
        assert_eq!(
            Store::open(&scratch.database(), None).err(),
            Some(SdkError::WrongKey)
        );

        let mut store = Store::open(&scratch.database(), Some(&key(1))).unwrap();
        store.change_key(Some(&key(2))).unwrap();
        drop(store);
        assert_eq!(
            Store::open(&scratch.database(), Some(&key(1))).err(),
            Some(SdkError::WrongKey)
        );

        let mut store = Store::open(&scratch.database(), Some(&key(2))).unwrap();
        store.change_key(None).unwrap();
        drop(store);
        let store = Store::open(&scratch.database(), None).unwrap();
        assert_eq!(store.messages(conversation, None, 10).unwrap().len(), 2);
        assert!(scratch.holds(&marker), "decrypted again");
    }

    #[test]
    fn a_key_is_refused_when_the_linked_sqlite_cannot_encrypt() {
        let missing = Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some("no such function: sqlite3mc_version".to_owned()),
        ));
        let error = check_encryption(missing).unwrap_err();
        assert_eq!(error, SdkError::EncryptionUnavailable);
        assert_eq!(error.code(), "encryption_unavailable");
        assert!(check_encryption(Ok("SQLite3 Multiple Ciphers".to_owned())).is_ok());
        assert_eq!(require_encryption(), Ok(()), "this build links SQLite3MC");
    }

    #[test]
    fn an_erase_that_cannot_truncate_the_log_is_an_error() {
        let scratch = Scratch::new();
        let mut store = Store::open(&scratch.database(), None).unwrap();
        store.bind_owner(UserId::new()).unwrap();
        let reader = Connection::open(scratch.database()).unwrap();
        reader
            .execute_batch("BEGIN; SELECT count(*) FROM meta;")
            .unwrap();
        assert!(matches!(store.erase_all(), Err(SdkError::Storage(_))));
        reader.execute_batch("COMMIT").unwrap();
        store.erase_all().unwrap();
    }

    #[test]
    fn an_interrupted_migration_leaves_version_zero_and_the_next_open_works() {
        let scratch = Scratch::new();
        let mut conn = Connection::open(scratch.database()).unwrap();
        let broken = "CREATE TABLE meta (key TEXT); CREATE TABLE half (x); CREATE TABLE half (x);";
        assert!(migrate(&mut conn, &[broken]).is_err());
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let tables: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert_eq!((version, tables), (0, 0));
        drop(conn);

        let mut store = Store::open(&scratch.database(), None).unwrap();
        let me = UserId::new();
        store.bind_owner(me).unwrap();
        assert_eq!(store.owner().unwrap(), Some(me));
    }

    #[test]
    fn migrations_build_on_the_previous_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        let steps = ["CREATE TABLE one (x)", "ALTER TABLE one ADD COLUMN y"];
        migrate(&mut conn, &steps[..1]).unwrap();
        migrate(&mut conn, &steps).unwrap();
        migrate(&mut conn, &steps).unwrap();
        conn.execute_batch("INSERT INTO one (x, y) VALUES (1, 2)")
            .unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let error = migrate(&mut conn, &steps[..1]).unwrap_err();
        assert!(matches!(error, SdkError::Storage(_)), "{error}");
    }

    #[test]
    fn debug_output_never_shows_a_body() {
        let mut f = fixture();
        let mut delivered = entry(f.conversation, f.peer, 1, EntryKind::Message);
        delivered.body = Some(b"a private text".to_vec());
        let (Applied::Added(message), _, _) = f.store.apply_deliver(&delivered).unwrap() else {
            panic!("a new message");
        };
        let shown = format!("{:?}", crate::model::Event::MessageAdded(message));
        assert!(!shown.contains("a private text"), "{shown}");
        assert!(shown.contains("<14 bytes>"), "{shown}");
    }

    pub(crate) const CARD: &str =
        r#"{"tpl":"agent.approval@1","params":{"title":"Deploy?"},"fallback":"Deploy?"}"#;

    pub(crate) fn card_entry(conversation: ConversationId, sender: UserId, seq: i64) -> Deliver {
        let mut card = entry(conversation, sender, seq, EntryKind::Message);
        card.content_type = ContentType::Rich.into();
        card.body = Some(CARD.as_bytes().to_vec());
        card
    }

    pub(crate) fn answer_entry(
        conversation: ConversationId,
        sender: UserId,
        seq: i64,
        card: &Deliver,
    ) -> Deliver {
        let mut answer = entry(conversation, sender, seq, EntryKind::Message);
        answer.content_type = ContentType::Answer.into();
        answer.target_seq = Some(card.seq);
        let card_id = Uuid::from_slice(&card.message_id).unwrap();
        answer.body = Some(
            format!(r#"{{"card":"{card_id}","values":["approve"],"fallback":"Approved"}}"#)
                .into_bytes(),
        );
        answer
    }

    #[test]
    fn cards_and_answers_keep_their_kind_and_an_answer_closes_its_card() {
        let mut f = fixture();
        let card = card_entry(f.conversation, f.peer, 1);
        let (applied, conversation, _) = f.store.apply_deliver(&card).unwrap();
        assert!(matches!(applied, Applied::Added(ref message)
            if message.kind == MessageKind::Rich && message.answer_seq.is_none()));
        assert_eq!(
            conversation.unwrap().unread,
            1,
            "a card from the peer is unread"
        );

        // The user answered on another device: the answer comes back as a delivery.
        let answer = answer_entry(f.conversation, f.me, 2, &card);
        let (applied, _, _) = f.store.apply_deliver(&answer).unwrap();
        let Applied::Added(answer) = applied else {
            panic!("a new message");
        };
        assert_eq!(
            (answer.kind, answer.card_seq, answer.answer_seq),
            (MessageKind::Answer, Some(1), None)
        );
        let card = f.store.mark_answered(&answer).unwrap().expect("closed");
        assert_eq!((card.seq, card.answer_seq), (Some(1), Some(2)));
        assert_eq!(f.store.mark_answered(&answer).unwrap(), None, "once");
        assert_eq!(message_at(&f, 1).answer_seq, Some(2));
        assert_eq!(message_at(&f, 2).kind, MessageKind::Answer);

        // A text message is never closed, whatever names it.
        f.store
            .apply_deliver(&entry(f.conversation, f.peer, 3, EntryKind::Message))
            .unwrap();
        let mut stray = answer;
        stray.card_seq = Some(3);
        assert_eq!(f.store.mark_answered(&stray).unwrap(), None);
    }

    #[test]
    fn answers_from_others_count_as_unread_and_history_carries_answer_seq() {
        let mut f = fixture();
        let mut card = card_entry(f.conversation, f.me, 1);
        card.answer_seq = Some(2);
        let answer = answer_entry(f.conversation, f.peer, 2, &card);
        let stored = f.store.store_history(&[card.clone(), answer]).unwrap();
        assert_eq!(stored[0].answer_seq, Some(2));
        assert_eq!(stored[1].card_seq, Some(1));
        let later = answer_entry(f.conversation, f.peer, 3, &card);
        let (_, conversation, _) = f.store.apply_deliver(&later).unwrap();
        assert_eq!(
            conversation.unwrap().unread,
            2,
            "both answers from the peer"
        );
    }

    #[test]
    fn a_database_from_the_first_schema_upgrades_and_keeps_its_messages() {
        let scratch = Scratch::new();
        let (me, peer, conversation) = (UserId::new(), UserId::new(), ConversationId::new());
        let mut conn = Connection::open(scratch.database()).unwrap();
        migrate(&mut conn, &SCHEMA_STEPS[..1]).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('owner', ?1)",
            params![bytes(me.0)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation (id, kind, peer_id) VALUES (?1, 1, ?2)",
            params![bytes(conversation.0), bytes(peer.0)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (conversation_id, client_msg_id, seq, sender_id, content_type, body)
             VALUES (?1, ?2, 1, ?3, 1, CAST('kept' AS BLOB))",
            params![bytes(conversation.0), bytes(Uuid::now_v7()), bytes(peer.0)],
        )
        .unwrap();
        drop(conn);

        let mut store = Store::open(&scratch.database(), None).unwrap();
        let held = store.messages(conversation, None, 10).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(
            (
                held[0].body.as_deref(),
                held[0].answer_seq,
                held[0].card_seq
            ),
            (Some("kept"), None, None)
        );
        store
            .apply_deliver(&card_entry(conversation, peer, 2))
            .unwrap();
        let held = store.messages(conversation, None, 10).unwrap();
        assert_eq!(held[1].kind, MessageKind::Rich);
    }

    #[test]
    fn contacts_follow_the_listing() {
        let mut f = fixture();
        let other = UserId::new();
        let accepted = Contact {
            user_id: f.peer,
            state: ContactState::Accepted,
        };
        assert!(f.store.set_contact(accepted).unwrap());
        assert!(!f.store.set_contact(accepted).unwrap());
        let asked = Contact {
            user_id: other,
            state: ContactState::RequestedIn,
        };
        let changed = f.store.replace_contacts(&[asked]).unwrap();
        assert_eq!(
            changed,
            [
                Contact {
                    user_id: f.peer,
                    state: ContactState::None
                },
                asked
            ]
        );
        assert_eq!(f.store.contacts().unwrap(), [asked]);
    }
}
