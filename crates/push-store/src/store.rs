//! [`PgStore`] and its transactions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, EntryKind, GroupPositions, InsertOutcome, InviteRecord,
    Member, MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::options::StoreOptions;
use crate::rows::{self, conversation_columns, entry_columns, member_columns};

/// The push-domain [`Store`] on PostgreSQL.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// A store over `pool`, whose database holds the migrated `messaging` schema (see
    /// [`PgStore::migrate`]).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connects a pool to `url` as `options` say. Every connection the pool opens first
    /// sets the lock, statement and idle-in-transaction timeouts. Fails if the options do
    /// not pass [`StoreOptions::validate`] or the first connection cannot be made.
    pub async fn connect(url: &str, options: &StoreOptions) -> Result<Self, sqlx::Error> {
        Ok(Self::new(crate::connect_pool(url, options).await?))
    }

    /// Applies the migrations the database does not have yet on this pool, whose statement
    /// and lock timeouts are sized for requests. Services do not use it: at start-up they
    /// call `push_store::migrate_database(url)` on a dedicated connection. Run it as the
    /// owner of the database: since PostgreSQL 15 other users cannot create objects in it.
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        crate::MIGRATOR.run(&self.pool).await
    }

    /// Checks that the database answers, for readiness probes.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// The pool the store runs on.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes every connection and waits until they are closed. Later calls fail.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// A READ COMMITTED transaction. Dropping it without [`Tx::commit`] rolls it back.
#[derive(Debug)]
pub struct PgTx {
    tx: Transaction<'static, Postgres>,
}

impl PgTx {
    fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }
}

#[async_trait]
impl Store for PgStore {
    type Tx = PgTx;

    async fn begin(&self) -> DomainResult<PgTx> {
        let tx = self
            .pool
            .begin_with("BEGIN ISOLATION LEVEL READ COMMITTED")
            .await
            .map_err(store_error)?;
        Ok(PgTx { tx })
    }
}

/// A count the domain keeps as `u32`, as the `integer` the schema stores.
fn to_integer(value: u32, what: &str) -> DomainResult<i32> {
    i32::try_from(value).map_err(|_| DomainError::Store(format!("{what} {value} is too large")))
}

/// Reads every row with `read`, failing like a query.
fn read_all<T>(
    rows: Vec<PgRow>,
    read: fn(&PgRow) -> Result<T, sqlx::Error>,
) -> DomainResult<Vec<T>> {
    rows.iter()
        .map(|row| read(row).map_err(store_error))
        .collect()
}

#[async_trait]
impl Tx for PgTx {
    async fn commit(self) -> DomainResult<()> {
        self.tx.commit().await.map_err(store_error)
    }

    async fn rollback(self) -> DomainResult<()> {
        self.tx.rollback().await.map_err(store_error)
    }

    async fn insert_user(&mut self, user: UserId) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.known_user (user_id) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(user.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn user_exists(&mut self, user: UserId) -> DomainResult<bool> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messaging.known_user WHERE user_id = $1)")
            .bind(user.0)
            .fetch_one(self.conn())
            .await
            .map_err(store_error)
    }

    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        // A concurrent insert of the same id makes this wait for its outcome, so the check
        // below sees whichever row won.
        sqlx::query(
            "INSERT INTO messaging.member_device (device_id, user_id) VALUES ($1, $2) \
             ON CONFLICT (device_id) DO NOTHING",
        )
        .bind(device.0)
        .bind(user.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        sqlx::query_scalar(
            "SELECT user_id = $2 AND revoked_at IS NULL \
             FROM messaging.member_device WHERE device_id = $1",
        )
        .bind(device.0)
        .bind(user.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)
    }

    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()> {
        sqlx::query(
            "UPDATE messaging.member_device SET revoked_at = coalesce(revoked_at, now()) \
             WHERE device_id = $1",
        )
        .bind(device.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn device_user(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM messaging.member_device WHERE device_id = $1")
                .bind(device.0)
                .fetch_optional(self.conn())
                .await
                .map_err(store_error)?;
        Ok(owner.map(UserId))
    }

    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>> {
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM messaging.member_device \
             WHERE device_id = $1 AND revoked_at IS NULL",
        )
        .bind(device.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        Ok(owner.map(UserId))
    }

    async fn devices_of_user(&mut self, user: UserId) -> DomainResult<Vec<DeviceId>> {
        let devices: Vec<Uuid> = sqlx::query_scalar(
            "SELECT device_id FROM messaging.member_device \
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(devices.into_iter().map(DeviceId).collect())
    }

    async fn lock_contact_pair(&mut self, a: UserId, b: UserId) -> DomainResult<()> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(locks::contact_pair(a, b))
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn lock_users(&mut self, users: &[UserId]) -> DomainResult<()> {
        let keys = locks::users(users);
        // A function scan over the array yields its elements in order, so the locks are
        // taken in the sorted order of their keys.
        sqlx::query("SELECT pg_advisory_xact_lock(key) FROM unnest($1::bigint[]) AS key")
            .bind(keys)
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn get_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
    ) -> DomainResult<Option<ContactState>> {
        let state: Option<i16> = sqlx::query_scalar(
            "SELECT state FROM messaging.contact WHERE owner_id = $1 AND peer_id = $2",
        )
        .bind(owner.0)
        .bind(peer.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        state
            .map(|state| decode(state).map_err(store_error))
            .transpose()
    }

    async fn put_contact(
        &mut self,
        owner: UserId,
        peer: UserId,
        state: ContactState,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.contact (owner_id, peer_id, state) VALUES ($1, $2, $3) \
             ON CONFLICT (owner_id, peer_id) \
             DO UPDATE SET state = excluded.state, sees_peer = false, updated_at = now()",
        )
        .bind(owner.0)
        .bind(peer.0)
        .bind(encode(state))
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn put_block(
        &mut self,
        owner: UserId,
        peer: UserId,
        sees_peer: bool,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.contact (owner_id, peer_id, state, sees_peer) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (owner_id, peer_id) \
             DO UPDATE SET state = excluded.state, sees_peer = excluded.sees_peer, \
                           updated_at = now()",
        )
        .bind(owner.0)
        .bind(peer.0)
        .bind(encode(ContactState::Blocked))
        .bind(sees_peer)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn delete_contact(&mut self, owner: UserId, peer: UserId) -> DomainResult<()> {
        sqlx::query("DELETE FROM messaging.contact WHERE owner_id = $1 AND peer_id = $2")
            .bind(owner.0)
            .bind(peer.0)
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn contacts_of(&mut self, owner: UserId) -> DomainResult<Vec<(UserId, ContactState)>> {
        let rows: Vec<(Uuid, i16)> =
            sqlx::query_as("SELECT peer_id, state FROM messaging.contact WHERE owner_id = $1")
                .bind(owner.0)
                .fetch_all(self.conn())
                .await
                .map_err(store_error)?;
        rows.into_iter()
            .map(|(peer, state)| Ok((UserId(peer), decode(state).map_err(store_error)?)))
            .collect()
    }

    async fn related_users(
        &mut self,
        user: UserId,
        candidates: &[UserId],
    ) -> DomainResult<Vec<UserId>> {
        let candidates: Vec<Uuid> = candidates.iter().map(|candidate| candidate.0).collect();
        // State 1 is requested_out: the user's own pending request grants nothing. State 4
        // is blocked: it counts only if the user could see the peer when blocking.
        let related: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT candidate FROM unnest($2::uuid[]) AS candidate \
             WHERE candidate <> $1 AND ( \
               EXISTS (SELECT 1 FROM messaging.contact \
                       WHERE owner_id = $1 AND peer_id = candidate AND state <> 1 \
                         AND (state <> 4 OR sees_peer)) \
               OR EXISTS (SELECT 1 FROM messaging.conversation_member mine \
                          JOIN messaging.conversation_member theirs USING (conversation_id) \
                          WHERE mine.user_id = $1 AND theirs.user_id = candidate))",
        )
        .bind(user.0)
        .bind(candidates)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(related.into_iter().map(UserId).collect())
    }

    async fn insert_invite(&mut self, invite: InviteRecord) -> DomainResult<InsertOutcome> {
        let inserted = sqlx::query(
            "INSERT INTO messaging.invite \
             (code, user_id, expires_at, max_uses, uses, revoked_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (code) DO NOTHING",
        )
        .bind(&invite.code)
        .bind(invite.user_id.0)
        .bind(invite.expires_at)
        .bind(to_integer(invite.max_uses, "max_uses")?)
        .bind(to_integer(invite.uses, "uses")?)
        .bind(invite.revoked_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(if inserted == 1 {
            InsertOutcome::Inserted
        } else {
            InsertOutcome::Duplicate
        })
    }

    async fn lock_invite(&mut self, code: &str) -> DomainResult<Option<InviteRecord>> {
        let row = sqlx::query(
            "SELECT code, user_id, expires_at, max_uses, uses, revoked_at \
             FROM messaging.invite WHERE code = $1 FOR UPDATE",
        )
        .bind(code)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::invite(&row).map_err(store_error))
            .transpose()
    }

    async fn update_invite(&mut self, invite: InviteRecord) -> DomainResult<()> {
        let updated =
            sqlx::query("UPDATE messaging.invite SET uses = $2, revoked_at = $3 WHERE code = $1")
                .bind(&invite.code)
                .bind(to_integer(invite.uses, "uses")?)
                .bind(invite.revoked_at)
                .execute(self.conn())
                .await
                .map_err(store_error)?
                .rows_affected();
        if updated == 0 {
            return Err(DomainError::Store(
                "update of an invite that does not exist".into(),
            ));
        }
        Ok(())
    }

    async fn find_direct(&mut self, key: [u8; 32]) -> DomainResult<Option<ConversationId>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT conversation_id FROM messaging.conversation WHERE direct_key = $1",
        )
        .bind(&key[..])
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        Ok(id.map(ConversationId))
    }

    async fn insert_conversation(
        &mut self,
        conversation: Conversation,
    ) -> DomainResult<InsertOutcome> {
        // Only a direct key conflict is a duplicate: a conversation id that exists still
        // fails with a primary key violation.
        let inserted = sqlx::query(
            "INSERT INTO messaging.conversation \
             (conversation_id, kind, direct_key, title, created_by, last_seq, created_at, \
              updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (direct_key) DO NOTHING",
        )
        .bind(conversation.id.0)
        .bind(encode(conversation.kind))
        .bind(conversation.direct_key.map(|key| key.to_vec()))
        .bind(&conversation.title)
        .bind(conversation.created_by.0)
        .bind(conversation.last_seq)
        .bind(conversation.created_at)
        .bind(conversation.updated_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(if inserted == 1 {
            InsertOutcome::Inserted
        } else {
            InsertOutcome::Duplicate
        })
    }

    async fn get_conversation(&mut self, id: ConversationId) -> DomainResult<Option<Conversation>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            " FROM messaging.conversation c WHERE c.conversation_id = $1"
        ))
        .bind(id.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::conversation(&row).map_err(store_error))
            .transpose()
    }

    async fn next_seq(&mut self, id: ConversationId, at: DateTime<Utc>) -> DomainResult<i64> {
        // The row lock this takes is held until the transaction ends: one appender at a
        // time per conversation.
        let seq: Option<i64> = sqlx::query_scalar(
            "UPDATE messaging.conversation \
             SET last_seq = last_seq + 1, updated_at = greatest(updated_at, $2) \
             WHERE conversation_id = $1 RETURNING last_seq",
        )
        .bind(id.0)
        .bind(at)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        seq.ok_or(DomainError::ConversationNotFound)
    }

    async fn set_title(&mut self, id: ConversationId, title: String) -> DomainResult<()> {
        let updated =
            sqlx::query("UPDATE messaging.conversation SET title = $2 WHERE conversation_id = $1")
                .bind(id.0)
                .bind(title)
                .execute(self.conn())
                .await
                .map_err(store_error)?
                .rows_affected();
        if updated == 0 {
            return Err(DomainError::ConversationNotFound);
        }
        Ok(())
    }

    async fn insert_member(&mut self, member: Member) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.conversation_member \
             (conversation_id, user_id, role, visible_from_seq, left_at_seq, read_seq) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (conversation_id, user_id) DO UPDATE SET \
             role = excluded.role, visible_from_seq = excluded.visible_from_seq, \
             left_at_seq = excluded.left_at_seq, read_seq = excluded.read_seq",
        )
        .bind(member.conversation_id.0)
        .bind(member.user_id.0)
        .bind(encode(member.role))
        .bind(member.visible_from_seq)
        .bind(member.left_at_seq)
        .bind(member.read_seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn get_member(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<Option<Member>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            member_columns!(),
            " FROM messaging.conversation_member m \
             WHERE m.conversation_id = $1 AND m.user_id = $2"
        ))
        .bind(conversation.0)
        .bind(user.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::member(&row).map_err(store_error))
            .transpose()
    }

    async fn members(&mut self, conversation: ConversationId) -> DomainResult<Vec<Member>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            member_columns!(),
            " FROM messaging.conversation_member m WHERE m.conversation_id = $1"
        ))
        .bind(conversation.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::member)
    }

    async fn memberships(&mut self, user: UserId) -> DomainResult<Vec<(Conversation, Member)>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            ", ",
            member_columns!(),
            " FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id \
             WHERE m.user_id = $1"
        ))
        .bind(user.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, |row| {
            Ok((rows::conversation(row)?, rows::member(row)?))
        })
    }

    async fn raise_read_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        seq: i64,
    ) -> DomainResult<bool> {
        let moved = sqlx::query(
            "UPDATE messaging.conversation_member SET read_seq = $3 \
             WHERE conversation_id = $1 AND user_id = $2 AND read_seq < $3",
        )
        .bind(conversation.0)
        .bind(user.0)
        .bind(seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        if moved == 1 {
            return Ok(true);
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messaging.conversation_member \
             WHERE conversation_id = $1 AND user_id = $2)",
        )
        .bind(conversation.0)
        .bind(user.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        if exists {
            Ok(false)
        } else {
            Err(DomainError::NotMember)
        }
    }

    async fn lock_conversation_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<()> {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(locks::conversation_positions(conversation))
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn insert_entry(&mut self, entry: Entry) -> DomainResult<InsertOutcome> {
        // Only a client_msg_id conflict is a duplicate: a seq or message id that exists
        // still fails with a unique violation.
        let inserted = sqlx::query(
            "INSERT INTO messaging.message \
             (conversation_id, seq, message_id, client_msg_id, kind, target_seq, sender_id, \
              sender_device_id, content_type, body, client_ts, server_ts, edited_at, edit_seq, \
              deleted_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
             ON CONFLICT (conversation_id, client_msg_id) DO NOTHING",
        )
        .bind(entry.conversation_id.0)
        .bind(entry.seq)
        .bind(entry.message_id.0)
        .bind(entry.client_msg_id.0)
        .bind(encode(entry.kind))
        .bind(entry.target_seq)
        .bind(entry.sender_id.0)
        .bind(entry.sender_device_id.map(|device| device.0))
        .bind(encode(entry.content_type))
        .bind(entry.body)
        .bind(entry.client_ts)
        .bind(entry.server_ts)
        .bind(entry.edited_at)
        .bind(entry.edit_seq)
        .bind(entry.deleted_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(if inserted == 1 {
            InsertOutcome::Inserted
        } else {
            InsertOutcome::Duplicate
        })
    }

    async fn entry_by_client_id(
        &mut self,
        conversation: ConversationId,
        client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            entry_columns!(),
            " FROM messaging.message e WHERE e.conversation_id = $1 AND e.client_msg_id = $2"
        ))
        .bind(conversation.0)
        .bind(client_msg_id.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::entry(&row).map_err(store_error))
            .transpose()
    }

    async fn entry_by_message_id(
        &mut self,
        conversation: ConversationId,
        message_id: MessageId,
    ) -> DomainResult<Option<Entry>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            entry_columns!(),
            " FROM messaging.message e WHERE e.conversation_id = $1 AND e.message_id = $2"
        ))
        .bind(conversation.0)
        .bind(message_id.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::entry(&row).map_err(store_error))
            .transpose()
    }

    async fn update_entry(&mut self, entry: Entry) -> DomainResult<()> {
        let updated = sqlx::query(
            "UPDATE messaging.message \
             SET body = $3, edited_at = $4, edit_seq = $5, deleted_at = $6 \
             WHERE conversation_id = $1 AND seq = $2",
        )
        .bind(entry.conversation_id.0)
        .bind(entry.seq)
        .bind(entry.body)
        .bind(entry.edited_at)
        .bind(entry.edit_seq)
        .bind(entry.deleted_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        if updated == 0 {
            return Err(DomainError::Store(
                "update of an entry that does not exist".into(),
            ));
        }
        Ok(())
    }

    async fn erase_edit_bodies(
        &mut self,
        conversation: ConversationId,
        target_seq: i64,
    ) -> DomainResult<()> {
        sqlx::query(
            "UPDATE messaging.message SET body = NULL \
             WHERE conversation_id = $1 AND target_seq = $2 AND kind = $3 \
             AND body IS NOT NULL",
        )
        .bind(conversation.0)
        .bind(target_seq)
        .bind(encode(EntryKind::Edit))
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn entries_after(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        targets_from: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            entry_columns!(),
            " FROM messaging.message e \
             WHERE e.conversation_id = $1 AND e.seq > $2 AND e.seq <= $3 \
             AND (e.target_seq IS NULL OR e.target_seq >= $4) \
             ORDER BY e.seq LIMIT $5"
        ))
        .bind(conversation.0)
        .bind(after)
        .bind(up_to)
        .bind(targets_from)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::entry)
    }

    async fn count_incoming_messages(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> DomainResult<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ( \
                SELECT 1 FROM messaging.message \
                WHERE conversation_id = $1 AND seq > $2 AND seq <= $3 \
                AND kind = 1 AND deleted_at IS NULL AND sender_id <> $4 \
                LIMIT $5 \
             ) incoming",
        )
        .bind(conversation.0)
        .bind(after)
        .bind(up_to)
        .bind(user.0)
        .bind(i64::from(cap))
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        u32::try_from(count).map_err(|_| DomainError::Store(format!("count {count} overflows")))
    }

    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        sqlx::query_scalar(
            "SELECT seq FROM messaging.message \
             WHERE conversation_id = $1 AND seq > $2 AND seq <= $3 \
             AND kind = 1 AND deleted_at IS NULL \
             AND ($4::uuid IS NULL OR sender_id <> $4) \
             ORDER BY seq LIMIT 1",
        )
        .bind(conversation.0)
        .bind(after)
        .bind(up_to)
        .bind(not_from.map(|user| user.0))
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)
    }

    async fn messages_before(
        &mut self,
        conversation: ConversationId,
        from: i64,
        before: i64,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        let rows = sqlx::query(concat!(
            "SELECT * FROM ( \
                SELECT ",
            entry_columns!(),
            " FROM messaging.message e \
                WHERE e.conversation_id = $1 AND e.seq >= $2 AND e.seq < $3 \
                AND e.kind IN (1, 4) \
                ORDER BY e.seq DESC LIMIT $4 \
             ) page ORDER BY seq"
        ))
        .bind(conversation.0)
        .bind(from)
        .bind(before)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::entry)
    }

    async fn get_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
    ) -> DomainResult<Option<i64>> {
        sqlx::query_scalar(
            "SELECT delivered_seq FROM messaging.device_cursor \
             WHERE device_id = $1 AND conversation_id = $2",
        )
        .bind(device.0)
        .bind(conversation.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)
    }

    async fn create_cursor_if_absent(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.device_cursor (device_id, conversation_id, delivered_seq) \
             VALUES ($1, $2, $3) ON CONFLICT (device_id, conversation_id) DO NOTHING",
        )
        .bind(device.0)
        .bind(conversation.0)
        .bind(seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn raise_cursor(
        &mut self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        let moved = sqlx::query(
            "INSERT INTO messaging.device_cursor (device_id, conversation_id, delivered_seq) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (device_id, conversation_id) \
             DO UPDATE SET delivered_seq = excluded.delivered_seq \
             WHERE device_cursor.delivered_seq < excluded.delivered_seq",
        )
        .bind(device.0)
        .bind(conversation.0)
        .bind(seq)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(moved == 1)
    }

    async fn delivered_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<i64> {
        sqlx::query_scalar("SELECT messaging.delivered_seq($1, $2)")
            .bind(conversation.0)
            .bind(user.0)
            .fetch_one(self.conn())
            .await
            .map_err(store_error)
    }

    async fn member_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            member_columns!(),
            ", messaging.delivered_seq(m.conversation_id, m.user_id) AS delivered_seq \
             FROM messaging.conversation_member m WHERE m.conversation_id = $1"
        ))
        .bind(conversation.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, |row| {
            Ok((rows::member(row)?, row.try_get("delivered_seq")?))
        })
    }

    async fn sync_positions(
        &mut self,
        user: UserId,
        device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            ", ",
            member_columns!(),
            ", position.seq AS position \
             FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id \
             LEFT JOIN messaging.device_cursor d \
                ON d.device_id = $2 AND d.conversation_id = m.conversation_id \
             CROSS JOIN LATERAL ( \
                SELECT greatest(coalesce(d.delivered_seq, 0), m.visible_from_seq - 1) AS seq \
             ) position \
             WHERE m.user_id = $1 AND position.seq < coalesce(m.left_at_seq, c.last_seq)"
        ))
        .bind(user.0)
        .bind(device.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, |row| {
            Ok((
                rows::conversation(row)?,
                rows::member(row)?,
                row.try_get("position")?,
            ))
        })
    }

    async fn group_positions(
        &mut self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>> {
        let positions: Option<(i64, i64)> =
            sqlx::query_as("SELECT delivered, read FROM messaging.group_positions($1, $2)")
                .bind(conversation.0)
                .bind(last_seq)
                .fetch_optional(self.conn())
                .await
                .map_err(store_error)?;
        Ok(positions.map(|(delivered, read)| GroupPositions { delivered, read }))
    }

    async fn conversation_overviews(
        &mut self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
        unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            conversation_columns!(),
            ", ",
            member_columns!(),
            ", others.members, others.admins, \
             peer.user_id AS peer_user_id, peer.role AS peer_role, \
             peer.visible_from_seq AS peer_visible_from_seq, \
             peer.left_at_seq AS peer_left_at_seq, peer.read_seq AS peer_read_seq, \
             peer.delivered_seq AS peer_delivered_seq, \
             unread.count AS unread, \
             gp.delivered AS group_delivered, gp.read AS group_read, \
             left_entry.server_ts AS left_at \
             FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id \
             CROSS JOIN LATERAL ( \
                SELECT \
                    coalesce(array_agg(o.user_id ORDER BY o.user_id) \
                        FILTER (WHERE o.left_at_seq IS NULL), '{}') AS members, \
                    coalesce(array_agg(o.user_id ORDER BY o.user_id) \
                        FILTER (WHERE o.left_at_seq IS NULL AND o.role = 1), '{}') AS admins \
                FROM messaging.conversation_member o \
                WHERE o.conversation_id = m.conversation_id \
             ) others \
             LEFT JOIN LATERAL ( \
                SELECT p.user_id, p.role, p.visible_from_seq, p.left_at_seq, p.read_seq, \
                    messaging.delivered_seq(p.conversation_id, p.user_id) AS delivered_seq \
                FROM messaging.conversation_member p \
                WHERE c.kind = 1 AND p.conversation_id = m.conversation_id \
                    AND p.user_id <> m.user_id \
                LIMIT 1 \
             ) peer ON true \
             CROSS JOIN LATERAL ( \
                SELECT count(*) AS count FROM ( \
                    SELECT 1 FROM messaging.message e \
                    WHERE e.conversation_id = m.conversation_id \
                        AND e.seq > greatest(m.read_seq, m.visible_from_seq - 1) \
                        AND e.seq <= coalesce(m.left_at_seq, c.last_seq) \
                        AND e.kind = 1 AND e.deleted_at IS NULL AND e.sender_id <> m.user_id \
                    LIMIT $3 \
                ) incoming \
             ) unread \
             LEFT JOIN LATERAL ( \
                SELECT g.delivered, g.read \
                FROM messaging.group_positions(c.conversation_id, c.last_seq) g \
                WHERE c.kind = 2 \
             ) gp ON true \
             LEFT JOIN messaging.message left_entry \
                ON left_entry.conversation_id = m.conversation_id \
                AND left_entry.seq = m.left_at_seq \
             WHERE m.user_id = $1 AND ($2::timestamptz IS NULL OR c.updated_at > $2)"
        ))
        .bind(user.0)
        .bind(changed_since)
        .bind(i64::from(unread_cap))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, overview)
    }
}

/// One row of the `conversation_overviews` query.
fn overview(row: &PgRow) -> Result<ConversationOverview, sqlx::Error> {
    let conversation = rows::conversation(row)?;
    let users = |column: &str| -> Result<Vec<UserId>, sqlx::Error> {
        let ids: Vec<Uuid> = row.try_get(column)?;
        Ok(ids.into_iter().map(UserId).collect())
    };
    let peer_user_id: Option<Uuid> = row.try_get("peer_user_id")?;
    let peer = match peer_user_id {
        Some(peer_user_id) => Some((
            Member {
                conversation_id: conversation.id,
                user_id: UserId(peer_user_id),
                role: decode(row.try_get("peer_role")?)?,
                visible_from_seq: row.try_get("peer_visible_from_seq")?,
                left_at_seq: row.try_get("peer_left_at_seq")?,
                read_seq: row.try_get("peer_read_seq")?,
            },
            row.try_get("peer_delivered_seq")?,
        )),
        None => None,
    };
    let delivered: Option<i64> = row.try_get("group_delivered")?;
    let read: Option<i64> = row.try_get("group_read")?;
    let unread: i64 = row.try_get("unread")?;
    Ok(ConversationOverview {
        member: rows::member(row)?,
        members: users("members")?,
        admins: users("admins")?,
        peer,
        unread: u32::try_from(unread)
            .map_err(|_| sqlx::Error::Decode(format!("unread count {unread}").into()))?,
        group: delivered
            .zip(read)
            .map(|(delivered, read)| GroupPositions { delivered, read }),
        left_at: row.try_get("left_at")?,
        conversation,
    })
}
