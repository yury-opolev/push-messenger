//! [`PgStore`] and its transactions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::calls::{CallDirection, CallHistoryRow, EndReason, HistoryCursor};
use push_domain::templates::TemplateRef;
use push_domain::{
    AgentPolicy, AgentSettings, ClientMsgId, ContactState, Conversation, ConversationFlags,
    ConversationId, ConversationOverview, DeviceId, DeviceState, DomainError, DomainResult, Entry,
    EntryKind, GroupPositions, InboundPolicy, InsertOutcome, InviteRecord, Member, MessageId,
    OutgoingAction, OutgoingError, OutgoingKind, OutgoingPolicy, OutgoingState, PublisherOwner,
    Store, StoredTemplate, TemplateEvent, TemplateStatus, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::codes::{Code, decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::options::StoreOptions;
use crate::rows::{
    self, conversation_columns, entry_columns, member_columns, outgoing_columns, template_columns,
};

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

    /// Reads a template version; `lock` is appended to the query (`""`, `" FOR SHARE"` or
    /// `" FOR UPDATE"`).
    async fn read_template(
        &mut self,
        template: &TemplateRef,
        lock: &'static str,
    ) -> DomainResult<Option<StoredTemplate>> {
        let sql = format!(
            "SELECT {} FROM messaging.template_version t \
             WHERE t.publisher = $1 AND t.name = $2 AND t.version = $3{lock}",
            template_columns!()
        );
        let row = sqlx::query(&sql)
            .bind(&template.publisher)
            .bind(&template.name)
            .bind(i32::from(template.version))
            .fetch_optional(self.conn())
            .await
            .map_err(store_error)?;
        row.map(|row| rows::template(&row).map_err(store_error))
            .transpose()
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
    async fn record_agent_owner(&mut self, agent: UserId, owner: UserId) -> DomainResult<bool> {
        let inserted = sqlx::query(
            "INSERT INTO messaging.agent_owner (agent_user_id, owner_user_id) VALUES ($1, $2) \
             ON CONFLICT DO NOTHING RETURNING 1",
        )
        .bind(agent.0)
        .bind(owner.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        let existing: Uuid = sqlx::query_scalar(
            "SELECT owner_user_id FROM messaging.agent_owner WHERE agent_user_id = $1",
        )
        .bind(agent.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        if existing != owner.0 {
            return Err(DomainError::Store("agent owner mismatch".into()));
        }
        Ok(inserted.is_some())
    }
    async fn commit(self) -> DomainResult<()> {
        self.tx.commit().await.map_err(store_error)
    }

    async fn rollback(self) -> DomainResult<()> {
        self.tx.rollback().await.map_err(store_error)
    }

    async fn savepoint(&mut self) -> DomainResult<()> {
        // A savepoint of the same name hides the earlier one; rolling back goes to the
        // latest.
        sqlx::query("SAVEPOINT push_domain")
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
    }

    async fn rollback_to_savepoint(&mut self) -> DomainResult<()> {
        sqlx::query("ROLLBACK TO SAVEPOINT push_domain")
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
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

    async fn record_password_reset(
        &mut self,
        user: UserId,
        at: DateTime<Utc>,
    ) -> DomainResult<DateTime<Utc>> {
        // greatest() skips a NULL, so the first reset is recorded as is.
        let kept: Option<DateTime<Utc>> = sqlx::query_scalar(
            "UPDATE messaging.known_user \
             SET password_reset_at = greatest(password_reset_at, $2) WHERE user_id = $1 \
             RETURNING password_reset_at",
        )
        .bind(user.0)
        .bind(at)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        kept.ok_or_else(|| {
            DomainError::Store("foreign key violation: user does not exist".to_owned())
        })
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

    async fn device_state(&mut self, device: DeviceId) -> DomainResult<Option<DeviceState>> {
        let row: Option<(Uuid, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT d.user_id, d.revoked_at IS NULL, u.password_reset_at \
             FROM messaging.member_device d JOIN messaging.known_user u USING (user_id) \
             WHERE d.device_id = $1",
        )
        .bind(device.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        Ok(row.map(|(owner, active, password_reset_at)| DeviceState {
            user: UserId(owner),
            active,
            password_reset_at,
        }))
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

    async fn presence_hidden(&mut self, users: &[UserId]) -> DomainResult<Vec<UserId>> {
        let users: Vec<Uuid> = users.iter().map(|user| user.0).collect();
        let hidden: Vec<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM messaging.presence_hidden WHERE user_id = ANY($1)",
        )
        .bind(users)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(hidden.into_iter().map(UserId).collect())
    }

    async fn set_presence_hidden(&mut self, user: UserId, hidden: bool) -> DomainResult<()> {
        let statement = if hidden {
            "INSERT INTO messaging.presence_hidden (user_id) VALUES ($1) ON CONFLICT DO NOTHING"
        } else {
            "DELETE FROM messaging.presence_hidden WHERE user_id = $1"
        };
        sqlx::query(statement)
            .bind(user.0)
            .execute(self.conn())
            .await
            .map_err(store_error)?;
        Ok(())
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
              updated_at, started_by_agent) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (direct_key) DO NOTHING",
        )
        .bind(conversation.id.0)
        .bind(encode(conversation.kind))
        .bind(conversation.direct_key.map(|key| key.to_vec()))
        .bind(&conversation.title)
        .bind(conversation.created_by.0)
        .bind(conversation.last_seq)
        .bind(conversation.created_at)
        .bind(conversation.updated_at)
        .bind(conversation.started_by_agent)
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
              deleted_at, answer_seq) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
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
        .bind(entry.answer_seq)
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
             SET body = $3, edited_at = $4, edit_seq = $5, deleted_at = $6, answer_seq = $7 \
             WHERE conversation_id = $1 AND seq = $2",
        )
        .bind(entry.conversation_id.0)
        .bind(entry.seq)
        .bind(entry.body)
        .bind(entry.edited_at)
        .bind(entry.edit_seq)
        .bind(entry.deleted_at)
        .bind(entry.answer_seq)
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
             AND (e.target_seq IS NULL OR e.kind NOT IN ($6, $7) OR e.target_seq >= $4) \
             ORDER BY e.seq LIMIT $5"
        ))
        .bind(conversation.0)
        .bind(after)
        .bind(up_to)
        .bind(targets_from)
        .bind(i64::from(limit))
        .bind(encode(EntryKind::Edit))
        .bind(encode(EntryKind::Delete))
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

    async fn set_conversation_hidden(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        hidden: bool,
        at: DateTime<Utc>,
    ) -> DomainResult<bool> {
        // Showing a conversation without a row changes nothing: no row means shown.
        let changed = sqlx::query(
            "INSERT INTO messaging.conversation_view (conversation_id, user_id, hidden, changed_at) \
             SELECT $1, $2, $3, $4 WHERE $3 \
             ON CONFLICT (conversation_id, user_id) DO UPDATE \
             SET hidden = EXCLUDED.hidden, changed_at = EXCLUDED.changed_at \
             WHERE conversation_view.hidden <> EXCLUDED.hidden",
        )
        .bind(conversation.0)
        .bind(user.0)
        .bind(hidden)
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        if changed > 0 {
            return Ok(true);
        }
        if !hidden {
            let updated = sqlx::query(
                "UPDATE messaging.conversation_view SET hidden = false, changed_at = $3 \
                 WHERE conversation_id = $1 AND user_id = $2 AND hidden",
            )
            .bind(conversation.0)
            .bind(user.0)
            .bind(at)
            .execute(self.conn())
            .await
            .map_err(store_error)?
            .rows_affected();
            if updated > 0 {
                return Ok(true);
            }
            let member: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM messaging.conversation_member \
                 WHERE conversation_id = $1 AND user_id = $2)",
            )
            .bind(conversation.0)
            .bind(user.0)
            .fetch_one(self.conn())
            .await
            .map_err(store_error)?;
            if !member {
                return Err(DomainError::Store(
                    "foreign key violation: member does not exist".to_owned(),
                ));
            }
        }
        Ok(false)
    }

    async fn unhide_conversation(
        &mut self,
        conversation: ConversationId,
        at: DateTime<Utc>,
    ) -> DomainResult<Vec<UserId>> {
        let mut shown: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE messaging.conversation_view SET hidden = false, changed_at = $2 \
             WHERE conversation_id = $1 AND hidden RETURNING user_id",
        )
        .bind(conversation.0)
        .bind(at)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        shown.sort();
        Ok(shown.into_iter().map(UserId).collect())
    }

    async fn conversation_flags(&mut self, user: UserId) -> DomainResult<Vec<ConversationFlags>> {
        let rows: Vec<(Uuid, bool, bool, DateTime<Utc>)> = sqlx::query_as(
            "SELECT m.conversation_id, coalesce(v.hidden, false), d.user_id IS NOT NULL, \
             greatest(v.changed_at, d.deleted_at) \
             FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id AND c.kind = 1 \
             LEFT JOIN messaging.conversation_view v \
                ON v.conversation_id = m.conversation_id AND v.user_id = m.user_id \
             LEFT JOIN messaging.conversation_member p \
                ON p.conversation_id = m.conversation_id AND p.user_id <> m.user_id \
             LEFT JOIN messaging.deleted_user d ON d.user_id = p.user_id \
             WHERE m.user_id = $1 AND (v.user_id IS NOT NULL OR d.user_id IS NOT NULL)",
        )
        .bind(user.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(
                |(conversation, hidden, peer_deleted, changed_at)| ConversationFlags {
                    conversation_id: ConversationId(conversation),
                    hidden,
                    peer_deleted,
                    changed_at,
                },
            )
            .collect())
    }

    async fn mark_user_deleted(&mut self, user: UserId, at: DateTime<Utc>) -> DomainResult<bool> {
        let inserted = sqlx::query(
            "INSERT INTO messaging.deleted_user (user_id, deleted_at) VALUES ($1, $2) \
             ON CONFLICT (user_id) DO NOTHING",
        )
        .bind(user.0)
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(inserted > 0)
    }

    async fn direct_peers(&mut self, user: UserId) -> DomainResult<Vec<(ConversationId, UserId)>> {
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT m.conversation_id, p.user_id \
             FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id AND c.kind = 1 \
             JOIN messaging.conversation_member p \
                ON p.conversation_id = m.conversation_id AND p.user_id <> m.user_id \
             WHERE m.user_id = $1",
        )
        .bind(user.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(conversation, peer)| (ConversationId(conversation), UserId(peer)))
            .collect())
    }

    async fn is_user_deleted(&mut self, user: UserId) -> DomainResult<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messaging.deleted_user WHERE user_id = $1)",
        )
        .bind(user.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)
    }

    async fn agent_policy(&mut self, agent: UserId) -> DomainResult<Option<AgentPolicy>> {
        let code: Option<i16> = sqlx::query_scalar(
            "SELECT policy FROM messaging.agent_policy WHERE agent_user_id = $1",
        )
        .bind(agent.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        code.map(|code| {
            AgentPolicy::from_code(code)
                .ok_or_else(|| DomainError::Store(format!("unknown agent policy {code}")))
        })
        .transpose()
    }

    async fn set_agent_policy(
        &mut self,
        agent: UserId,
        policy: AgentPolicy,
        at: DateTime<Utc>,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.agent_policy (agent_user_id, policy, changed_at) \
             VALUES ($1, $2, $3) ON CONFLICT (agent_user_id) \
             DO UPDATE SET policy = excluded.policy, changed_at = excluded.changed_at",
        )
        .bind(agent.0)
        .bind(policy.code())
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn forget_agent(&mut self, agent: UserId) -> DomainResult<()> {
        sqlx::query(
            "WITH policy AS (DELETE FROM messaging.agent_policy WHERE agent_user_id = $1), \
             settings AS (DELETE FROM messaging.agent_settings WHERE agent_user_id = $1), \
             actions AS (DELETE FROM messaging.outgoing_action WHERE agent_user_id = $1), \
             action_log AS (DELETE FROM messaging.agent_action_log WHERE agent_user_id = $1) \
             DELETE FROM messaging.agent_request_log WHERE agent_user_id = $1",
        )
        .bind(agent.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn waiting_requests(
        &mut self,
        agent: UserId,
    ) -> DomainResult<Vec<(UserId, DateTime<Utc>)>> {
        let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
            "SELECT peer_id, updated_at FROM messaging.contact \
             WHERE owner_id = $1 AND state = $2 ORDER BY updated_at, peer_id",
        )
        .bind(agent.0)
        .bind(encode(ContactState::RequestedIn))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(peer, at)| (UserId(peer), at))
            .collect())
    }

    async fn waiting_in_count(&mut self, agent: UserId) -> DomainResult<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM messaging.contact WHERE owner_id = $1 AND state = $2",
        )
        .bind(agent.0)
        .bind(encode(ContactState::RequestedIn))
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        count_u32(count)
    }

    async fn outgoing_counts(&mut self, user: UserId) -> DomainResult<(u32, u32)> {
        let (pending, waiting): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE EXISTS ( \
                 SELECT 1 FROM messaging.contact back \
                 JOIN messaging.agent_owner agent ON agent.agent_user_id = back.owner_id \
                 WHERE back.owner_id = mine.peer_id AND back.peer_id = mine.owner_id \
                 AND back.state = $3)) \
             FROM messaging.contact mine WHERE mine.owner_id = $1 AND mine.state = $2",
        )
        .bind(user.0)
        .bind(encode(ContactState::RequestedOut))
        .bind(encode(ContactState::RequestedIn))
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        Ok((count_u32(pending)?, count_u32(waiting)?))
    }

    async fn put_refusal(
        &mut self,
        requester: UserId,
        agent: UserId,
        release_at: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let inserted = sqlx::query(
            "INSERT INTO messaging.refused_request (requester_id, agent_user_id, release_at) \
             VALUES ($1, $2, $3) ON CONFLICT (requester_id, agent_user_id) DO NOTHING",
        )
        .bind(requester.0)
        .bind(agent.0)
        .bind(release_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(inserted > 0)
    }

    async fn delete_refusal(&mut self, requester: UserId, agent: UserId) -> DomainResult<bool> {
        let deleted = sqlx::query(
            "DELETE FROM messaging.refused_request WHERE requester_id = $1 AND agent_user_id = $2",
        )
        .bind(requester.0)
        .bind(agent.0)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(deleted > 0)
    }

    async fn due_refusals(
        &mut self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> DomainResult<Vec<(UserId, UserId)>> {
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT requester_id, agent_user_id FROM messaging.refused_request \
             WHERE release_at <= $1 ORDER BY release_at, requester_id, agent_user_id LIMIT $2",
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(requester, agent)| (UserId(requester), UserId(agent)))
            .collect())
    }

    async fn log_agent_request(
        &mut self,
        agent: UserId,
        peer: UserId,
        at: DateTime<Utc>,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.agent_request_log (agent_user_id, peer_id, sent_at) \
             VALUES ($1, $2, $3)",
        )
        .bind(agent.0)
        .bind(peer.0)
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn agent_requests_since(
        &mut self,
        agent: UserId,
        since: DateTime<Utc>,
    ) -> DomainResult<(u32, Option<DateTime<Utc>>)> {
        let (count, oldest): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT count(*), min(sent_at) FROM messaging.agent_request_log \
             WHERE agent_user_id = $1 AND sent_at >= $2",
        )
        .bind(agent.0)
        .bind(since)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        Ok((count_u32(count)?, oldest))
    }

    async fn prune_agent_requests(&mut self, before: DateTime<Utc>) -> DomainResult<u64> {
        Ok(
            sqlx::query("DELETE FROM messaging.agent_request_log WHERE sent_at < $1")
                .bind(before)
                .execute(self.conn())
                .await
                .map_err(store_error)?
                .rows_affected(),
        )
    }

    async fn agent_settings(&mut self, agent: UserId) -> DomainResult<AgentSettings> {
        let row: Option<(i16, i16, i16, i16, i16)> = sqlx::query_as(
            "SELECT inbound, requests, chats, groups, calls FROM messaging.agent_settings \
             WHERE agent_user_id = $1",
        )
        .bind(agent.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        let Some((inbound, requests, chats, groups, calls)) = row else {
            return Ok(AgentSettings::default());
        };
        let outgoing = |code: i16| {
            OutgoingPolicy::from_code(code)
                .ok_or_else(|| DomainError::Store(format!("unknown outgoing policy {code}")))
        };
        Ok(AgentSettings {
            inbound: InboundPolicy::from_code(inbound)
                .ok_or_else(|| DomainError::Store(format!("unknown inbound policy {inbound}")))?,
            requests: outgoing(requests)?,
            chats: outgoing(chats)?,
            groups: outgoing(groups)?,
            calls: outgoing(calls)?,
        })
    }

    async fn set_agent_settings(
        &mut self,
        agent: UserId,
        settings: &AgentSettings,
        at: DateTime<Utc>,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.agent_settings \
             (agent_user_id, inbound, requests, chats, groups, calls, changed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (agent_user_id) \
             DO UPDATE SET inbound = excluded.inbound, requests = excluded.requests, \
                chats = excluded.chats, groups = excluded.groups, calls = excluded.calls, \
                changed_at = excluded.changed_at",
        )
        .bind(agent.0)
        .bind(settings.inbound.code())
        .bind(settings.requests.code())
        .bind(settings.chats.code())
        .bind(settings.groups.code())
        .bind(settings.calls.code())
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn insert_outgoing(&mut self, action: &OutgoingAction) -> DomainResult<()> {
        let members: Vec<Uuid> = action.members.iter().map(|member| member.0).collect();
        sqlx::query(
            "INSERT INTO messaging.outgoing_action \
             (action_id, agent_user_id, kind, target_user_id, conversation_id, title, \
              member_ids, state, error_code, created_at, resolved_at, used_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(action.action_id)
        .bind(action.agent.0)
        .bind(action.kind.code())
        .bind(action.target.map(|target| target.0))
        .bind(action.conversation.map(|conversation| conversation.0))
        .bind(&action.title)
        .bind(members)
        .bind(action.state.code())
        .bind(action.error.map(OutgoingError::code))
        .bind(action.created_at)
        .bind(action.resolved_at)
        .bind(action.used_at)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn find_waiting_outgoing(
        &mut self,
        action: &OutgoingAction,
    ) -> DomainResult<Option<OutgoingAction>> {
        let members: Vec<Uuid> = action.members.iter().map(|member| member.0).collect();
        let row = sqlx::query(concat!(
            "SELECT ",
            outgoing_columns!(),
            " FROM messaging.outgoing_action \
             WHERE agent_user_id = $1 AND state = 1 AND kind = $2 \
               AND target_user_id IS NOT DISTINCT FROM $3 \
               AND conversation_id IS NOT DISTINCT FROM $4 \
               AND title IS NOT DISTINCT FROM $5 AND member_ids = $6 \
             ORDER BY created_at, action_id LIMIT 1"
        ))
        .bind(action.agent.0)
        .bind(action.kind.code())
        .bind(action.target.map(|target| target.0))
        .bind(action.conversation.map(|conversation| conversation.0))
        .bind(&action.title)
        .bind(members)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::outgoing(&row).map_err(store_error))
            .transpose()
    }

    async fn outgoing(&mut self, action_id: Uuid) -> DomainResult<Option<OutgoingAction>> {
        let row = sqlx::query(concat!(
            "SELECT ",
            outgoing_columns!(),
            " FROM messaging.outgoing_action WHERE action_id = $1"
        ))
        .bind(action_id)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::outgoing(&row).map_err(store_error))
            .transpose()
    }

    async fn waiting_outgoing_count(&mut self, agent: UserId) -> DomainResult<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM messaging.outgoing_action \
             WHERE agent_user_id = $1 AND state = 1",
        )
        .bind(agent.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        count_u32(count)
    }

    async fn list_outgoing(
        &mut self,
        agent: UserId,
        resolved_since: Option<DateTime<Utc>>,
    ) -> DomainResult<Vec<OutgoingAction>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            outgoing_columns!(),
            " FROM messaging.outgoing_action \
             WHERE agent_user_id = $1 AND (state = 1 OR resolved_at >= $2) \
             ORDER BY created_at, action_id"
        ))
        .bind(agent.0)
        .bind(resolved_since)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::outgoing)
    }

    async fn resolve_outgoing(
        &mut self,
        action_id: Uuid,
        from: OutgoingState,
        to: OutgoingState,
        error: Option<OutgoingError>,
        conversation: Option<ConversationId>,
        at: DateTime<Utc>,
    ) -> DomainResult<Option<OutgoingAction>> {
        let row = sqlx::query(concat!(
            "UPDATE messaging.outgoing_action \
             SET state = $3, error_code = $4, \
                 conversation_id = coalesce($5, conversation_id), resolved_at = $6 \
             WHERE action_id = $1 AND state = $2 RETURNING ",
            outgoing_columns!()
        ))
        .bind(action_id)
        .bind(from.code())
        .bind(to.code())
        .bind(error.map(OutgoingError::code))
        .bind(conversation.map(|conversation| conversation.0))
        .bind(at)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        row.map(|row| rows::outgoing(&row).map_err(store_error))
            .transpose()
    }

    async fn approved_call_grant(
        &mut self,
        agent: UserId,
        callee: UserId,
        since: DateTime<Utc>,
    ) -> DomainResult<Option<Uuid>> {
        sqlx::query_scalar(
            "SELECT action_id FROM messaging.outgoing_action \
             WHERE agent_user_id = $1 AND kind = $2 AND target_user_id = $3 AND state = $4 \
               AND used_at IS NULL AND resolved_at >= $5 \
             ORDER BY resolved_at, action_id LIMIT 1",
        )
        .bind(agent.0)
        .bind(OutgoingKind::Call.code())
        .bind(callee.0)
        .bind(OutgoingState::Approved.code())
        .bind(since)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)
    }

    async fn use_outgoing(&mut self, action_id: Uuid, at: DateTime<Utc>) -> DomainResult<bool> {
        let used = sqlx::query(
            "UPDATE messaging.outgoing_action SET used_at = $2 \
             WHERE action_id = $1 AND used_at IS NULL",
        )
        .bind(action_id)
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(used == 1)
    }

    async fn expire_outgoing(
        &mut self,
        before: DateTime<Utc>,
        at: DateTime<Utc>,
        limit: u32,
    ) -> DomainResult<Vec<OutgoingAction>> {
        // A row resolved meanwhile fails `state = 1` when the UPDATE rechecks it.
        let rows = sqlx::query(concat!(
            "UPDATE messaging.outgoing_action SET state = $3, resolved_at = $2 \
             WHERE state = 1 AND action_id IN ( \
                SELECT action_id FROM messaging.outgoing_action \
                WHERE state = 1 AND created_at <= $1 \
                ORDER BY created_at, action_id LIMIT $4) \
             RETURNING ",
            outgoing_columns!()
        ))
        .bind(before)
        .bind(at)
        .bind(OutgoingState::Expired.code())
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        let mut expired = read_all(rows, rows::outgoing)?;
        expired.sort_by_key(|action| (action.created_at, action.action_id));
        Ok(expired)
    }

    async fn prune_outgoing(&mut self, before: DateTime<Utc>) -> DomainResult<u64> {
        Ok(sqlx::query(
            "DELETE FROM messaging.outgoing_action WHERE state <> 1 AND resolved_at < $1",
        )
        .bind(before)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected())
    }

    async fn log_agent_action(
        &mut self,
        agent: UserId,
        kind: OutgoingKind,
        count: u32,
        at: DateTime<Utc>,
    ) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.agent_action_log (agent_user_id, kind, count, at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(agent.0)
        .bind(kind.code())
        .bind(to_integer(count, "count")?)
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn agent_actions_since(
        &mut self,
        agent: UserId,
        kind: OutgoingKind,
        since: DateTime<Utc>,
    ) -> DomainResult<(u32, Option<DateTime<Utc>>)> {
        let (count, oldest): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT coalesce(sum(count), 0)::bigint, min(at) FROM messaging.agent_action_log \
             WHERE agent_user_id = $1 AND kind = $2 AND at >= $3",
        )
        .bind(agent.0)
        .bind(kind.code())
        .bind(since)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        Ok((count_u32(count)?, oldest))
    }

    async fn prune_agent_actions(&mut self, before: DateTime<Utc>) -> DomainResult<u64> {
        Ok(
            sqlx::query("DELETE FROM messaging.agent_action_log WHERE at < $1")
                .bind(before)
                .execute(self.conn())
                .await
                .map_err(store_error)?
                .rows_affected(),
        )
    }

    async fn message_counts(&mut self, user: UserId) -> DomainResult<Vec<(ConversationId, i64)>> {
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT m.conversation_id, count(*) FROM messaging.conversation_member m \
             JOIN messaging.conversation c ON c.conversation_id = m.conversation_id \
             JOIN messaging.message e ON e.conversation_id = m.conversation_id \
              AND e.seq >= m.visible_from_seq AND e.seq <= coalesce(m.left_at_seq, c.last_seq) \
             WHERE m.user_id = $1 AND e.kind = 1 AND e.deleted_at IS NULL \
             GROUP BY m.conversation_id",
        )
        .bind(user.0)
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        Ok(rows
            .into_iter()
            .map(|(conversation, count)| (ConversationId(conversation), count))
            .collect())
    }

    async fn is_agent(&mut self, user: UserId) -> DomainResult<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM messaging.agent_owner WHERE agent_user_id = $1)",
        )
        .bind(user.0)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)
    }

    async fn agent_owner(&mut self, user: UserId) -> DomainResult<Option<UserId>> {
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT owner_user_id FROM messaging.agent_owner WHERE agent_user_id = $1",
        )
        .bind(user.0)
        .fetch_optional(self.conn())
        .await
        .map_err(store_error)?;
        Ok(owner.map(UserId))
    }

    async fn template(&mut self, template: &TemplateRef) -> DomainResult<Option<StoredTemplate>> {
        self.read_template(template, "").await
    }

    async fn template_for_send(
        &mut self,
        template: &TemplateRef,
    ) -> DomainResult<Option<StoredTemplate>> {
        self.read_template(template, " FOR SHARE").await
    }

    async fn lock_template(
        &mut self,
        template: &TemplateRef,
    ) -> DomainResult<Option<StoredTemplate>> {
        self.read_template(template, " FOR UPDATE").await
    }

    async fn insert_template(&mut self, template: StoredTemplate) -> DomainResult<InsertOutcome> {
        let inserted = sqlx::query(
            "INSERT INTO messaging.template_version \
             (publisher, name, version, title, definition, status, submitted_by, submitted_at, \
              reviewed_at, change_seq) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (publisher, name, version) DO NOTHING",
        )
        .bind(&template.template.publisher)
        .bind(&template.template.name)
        .bind(i32::from(template.template.version))
        .bind(&template.title)
        .bind(&template.definition)
        .bind(encode(template.status))
        .bind(template.submitted_by.map(|user| user.0))
        .bind(template.submitted_at)
        .bind(template.reviewed_at)
        .bind(template.change_seq)
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

    async fn set_template_status(
        &mut self,
        template: &TemplateRef,
        status: TemplateStatus,
        at: DateTime<Utc>,
        change_seq: Option<i64>,
    ) -> DomainResult<()> {
        let refused = || {
            DomainError::Store(format!(
                "no template {template} whose status may become {}",
                status.as_str()
            ))
        };
        // Each status can be reached from at most one other.
        let from = <TemplateStatus as Code>::CODES
            .iter()
            .map(|(from, _)| *from)
            .find(|from| from.can_become(status))
            .ok_or_else(refused)?;
        let updated = sqlx::query(
            "UPDATE messaging.template_version \
             SET status = $4, reviewed_at = $5, change_seq = $6 \
             WHERE publisher = $1 AND name = $2 AND version = $3 AND status = $7",
        )
        .bind(&template.publisher)
        .bind(&template.name)
        .bind(i32::from(template.version))
        .bind(encode(status))
        .bind(at)
        .bind(change_seq)
        .bind(encode(from))
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        if updated == 0 {
            return Err(refused());
        }
        Ok(())
    }

    async fn next_template_change(&mut self) -> DomainResult<i64> {
        sqlx::query_scalar(
            "UPDATE messaging.template_change SET last_seq = last_seq + 1 RETURNING last_seq",
        )
        .fetch_one(self.conn())
        .await
        .map_err(store_error)
    }

    async fn last_template_change(&mut self) -> DomainResult<i64> {
        // The migration inserts the single counter row: a missing row is a broken schema.
        sqlx::query_scalar("SELECT last_seq FROM messaging.template_change")
            .fetch_one(self.conn())
            .await
            .map_err(store_error)
    }

    async fn templates_changed_since(
        &mut self,
        since: i64,
        limit: u32,
    ) -> DomainResult<Vec<StoredTemplate>> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            template_columns!(),
            " FROM messaging.template_version t \
             WHERE t.change_seq > $1 AND t.status IN ($2, $3) \
             ORDER BY t.change_seq LIMIT $4"
        ))
        .bind(since)
        .bind(encode(TemplateStatus::Approved))
        .bind(encode(TemplateStatus::Revoked))
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::template)
    }

    async fn templates_with_status(
        &mut self,
        status: Option<TemplateStatus>,
        limit: u32,
    ) -> DomainResult<Vec<StoredTemplate>> {
        // Ties sort by ref byte by byte (COLLATE "C"), as the in-memory store does.
        let rows = sqlx::query(concat!(
            "SELECT ",
            template_columns!(),
            " FROM messaging.template_version t \
             WHERE $1::smallint IS NULL OR t.status = $1 \
             ORDER BY t.submitted_at DESC, t.publisher COLLATE \"C\", t.name COLLATE \"C\", \
              t.version \
             LIMIT $2"
        ))
        .bind(status.map(encode))
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::template)
    }

    async fn pending_templates_of(&mut self, user: UserId) -> DomainResult<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM messaging.template_version \
             WHERE submitted_by = $1 AND status = $2",
        )
        .bind(user.0)
        .bind(encode(TemplateStatus::Pending))
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        u32::try_from(count)
            .map_err(|_| DomainError::Store(format!("pending template count {count}")))
    }

    async fn claim_publisher(
        &mut self,
        publisher: &str,
        owner: PublisherOwner,
    ) -> DomainResult<PublisherOwner> {
        let owner_id = match owner {
            PublisherOwner::Server => None,
            PublisherOwner::User(user) => Some(user.0),
        };
        // The insert waits for a concurrent claim of the same publisher to end; the
        // select then locks whichever row won.
        sqlx::query(
            "INSERT INTO messaging.template_publisher (publisher, owner_user_id) \
             VALUES ($1, $2) ON CONFLICT (publisher) DO NOTHING",
        )
        .bind(publisher)
        .bind(owner_id)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        let recorded: Option<Uuid> = sqlx::query_scalar(
            "SELECT owner_user_id FROM messaging.template_publisher \
             WHERE publisher = $1 FOR UPDATE",
        )
        .bind(publisher)
        .fetch_one(self.conn())
        .await
        .map_err(store_error)?;
        Ok(recorded.map_or(PublisherOwner::Server, |user| {
            PublisherOwner::User(UserId(user))
        }))
    }

    async fn insert_template_event(&mut self, event: TemplateEvent) -> DomainResult<()> {
        sqlx::query(
            "INSERT INTO messaging.template_event \
             (publisher, name, version, action, actor, reviewer, note, at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&event.template.publisher)
        .bind(&event.template.name)
        .bind(i32::from(event.template.version))
        .bind(event.action.as_str())
        .bind(event.actor.map(|user| user.0))
        .bind(event.reviewer)
        .bind(event.note)
        .bind(event.at)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn template_events(
        &mut self,
        template: &TemplateRef,
    ) -> DomainResult<Vec<TemplateEvent>> {
        let rows = sqlx::query(
            "SELECT publisher, name, version, action, actor, reviewer, note, at \
             FROM messaging.template_event \
             WHERE publisher = $1 AND name = $2 AND version = $3 \
             ORDER BY event_id",
        )
        .bind(&template.publisher)
        .bind(&template.name)
        .bind(i32::from(template.version))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, rows::template_event)
    }

    async fn insert_call_history(&mut self, rows: &[CallHistoryRow]) -> DomainResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let users: Vec<Uuid> = rows.iter().map(|row| row.user.0).collect();
        let calls: Vec<Uuid> = rows.iter().map(|row| row.call_id).collect();
        let peers: Vec<Uuid> = rows.iter().map(|row| row.peer.0).collect();
        let directions: Vec<&str> = rows.iter().map(|row| direction(row.direction)).collect();
        let started: Vec<DateTime<Utc>> = rows.iter().map(|row| row.started_at).collect();
        let answered: Vec<Option<DateTime<Utc>>> = rows.iter().map(|row| row.answered_at).collect();
        let ended: Vec<DateTime<Utc>> = rows.iter().map(|row| row.ended_at).collect();
        let durations = rows
            .iter()
            .map(|row| to_integer(row.duration_s, "duration_s"))
            .collect::<DomainResult<Vec<i32>>>()?;
        let outcomes: Vec<&str> = rows.iter().map(|row| row.outcome.as_str()).collect();
        let by_agent: Vec<bool> = rows.iter().map(|row| row.started_by_agent).collect();
        sqlx::query(
            "INSERT INTO messaging.call_history \
             (user_id, call_id, peer_user_id, direction, started_at, answered_at, ended_at, \
              duration_s, outcome, started_by_agent) \
             SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::text[], \
                $5::timestamptz[], $6::timestamptz[], $7::timestamptz[], $8::integer[], \
                $9::text[], $10::boolean[]) \
             ON CONFLICT (user_id, call_id) DO NOTHING",
        )
        .bind(users)
        .bind(calls)
        .bind(peers)
        .bind(directions)
        .bind(started)
        .bind(answered)
        .bind(ended)
        .bind(durations)
        .bind(outcomes)
        .bind(by_agent)
        .execute(self.conn())
        .await
        .map_err(store_error)?;
        Ok(())
    }

    async fn call_history(
        &mut self,
        user: UserId,
        before: Option<HistoryCursor>,
        since: Option<HistoryCursor>,
        limit: u32,
    ) -> DomainResult<Vec<CallHistoryRow>> {
        let rows = sqlx::query(
            "SELECT user_id, call_id, peer_user_id, direction, started_at, answered_at, \
                    ended_at, duration_s, outcome, started_by_agent \
             FROM messaging.call_history \
             WHERE user_id = $1 AND hidden_at IS NULL \
               AND ($2::timestamptz IS NULL OR (ended_at, call_id) < ($2, $3::uuid)) \
               AND ($4::timestamptz IS NULL OR (ended_at, call_id) > ($4, $5::uuid)) \
             ORDER BY ended_at DESC, call_id DESC \
             LIMIT $6",
        )
        .bind(user.0)
        .bind(before.map(|cursor| cursor.ended_at))
        .bind(before.map(|cursor| cursor.call_id))
        .bind(since.map(|cursor| cursor.ended_at))
        .bind(since.map(|cursor| cursor.call_id))
        .bind(i64::from(limit))
        .fetch_all(self.conn())
        .await
        .map_err(store_error)?;
        read_all(rows, call_history_row)
    }

    async fn hide_call(
        &mut self,
        user: UserId,
        call: Uuid,
        at: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let hidden = sqlx::query(
            "UPDATE messaging.call_history SET hidden_at = $3 \
             WHERE user_id = $1 AND call_id = $2 AND hidden_at IS NULL",
        )
        .bind(user.0)
        .bind(call)
        .bind(at)
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(hidden == 1)
    }

    async fn delete_call_history_before(
        &mut self,
        cutoff: DateTime<Utc>,
        batch: u32,
    ) -> DomainResult<u64> {
        let deleted = sqlx::query(
            "DELETE FROM messaging.call_history WHERE (user_id, call_id) IN ( \
                SELECT user_id, call_id FROM messaging.call_history \
                WHERE ended_at < $1 LIMIT $2)",
        )
        .bind(cutoff)
        .bind(i64::from(batch))
        .execute(self.conn())
        .await
        .map_err(store_error)?
        .rows_affected();
        Ok(deleted)
    }
}

fn direction(direction: CallDirection) -> &'static str {
    match direction {
        CallDirection::In => "in",
        CallDirection::Out => "out",
    }
}

/// One row of `messaging.call_history`.
fn call_history_row(row: &PgRow) -> Result<CallHistoryRow, sqlx::Error> {
    let decode_error = |column: &str, value: &str| sqlx::Error::ColumnDecode {
        index: column.to_owned(),
        source: format!("unknown value {value:?}").into(),
    };
    let direction: String = row.try_get("direction")?;
    let direction = match direction.as_str() {
        "in" => CallDirection::In,
        "out" => CallDirection::Out,
        other => return Err(decode_error("direction", other)),
    };
    let outcome: String = row.try_get("outcome")?;
    let outcome = [
        EndReason::Completed,
        EndReason::Missed,
        EndReason::Declined,
        EndReason::Cancelled,
        EndReason::Busy,
        EndReason::Failed,
        EndReason::Unreachable,
    ]
    .into_iter()
    .find(|reason| reason.as_str() == outcome)
    .ok_or_else(|| decode_error("outcome", &outcome))?;
    let duration_s: i32 = row.try_get("duration_s")?;
    Ok(CallHistoryRow {
        call_id: row.try_get("call_id")?,
        user: UserId(row.try_get("user_id")?),
        peer: UserId(row.try_get("peer_user_id")?),
        direction,
        started_at: row.try_get("started_at")?,
        answered_at: row.try_get("answered_at")?,
        ended_at: row.try_get("ended_at")?,
        duration_s: u32::try_from(duration_s).map_err(|_| sqlx::Error::ColumnDecode {
            index: "duration_s".to_owned(),
            source: "negative".into(),
        })?,
        outcome,
        started_by_agent: row.try_get("started_by_agent")?,
    })
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

/// A `count(*)` as `u32`.
fn count_u32(count: i64) -> DomainResult<u32> {
    u32::try_from(count).map_err(|_| DomainError::Store(format!("count {count} overflows")))
}
