//! [`PgStore`] and its transactions.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use push_domain::{
    ClientMsgId, ContactState, Conversation, ConversationId, ConversationOverview, DeviceId,
    DomainError, DomainResult, Entry, GroupPositions, InsertOutcome, InviteRecord, Member,
    MessageId, Store, Tx, UserId,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::codes::{decode, encode};
use crate::error::store_error;
use crate::locks;
use crate::rows::{self, conversation_columns, member_columns};

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

    /// Applies the migrations the database does not have yet.
    pub async fn migrate(&self) -> Result<(), sqlx::migrate::MigrateError> {
        crate::MIGRATOR.run(&self.pool).await
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
             DO UPDATE SET state = excluded.state, updated_at = now()",
        )
        .bind(owner.0)
        .bind(peer.0)
        .bind(encode(state))
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

    async fn insert_entry(&mut self, _entry: Entry) -> DomainResult<InsertOutcome> {
        unimplemented!("plan 3B task 4")
    }

    async fn entry_by_client_id(
        &mut self,
        _conversation: ConversationId,
        _client_msg_id: ClientMsgId,
    ) -> DomainResult<Option<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn entry_by_message_id(
        &mut self,
        _conversation: ConversationId,
        _message_id: MessageId,
    ) -> DomainResult<Option<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn update_entry(&mut self, _entry: Entry) -> DomainResult<()> {
        unimplemented!("plan 3B task 4")
    }

    async fn erase_edit_bodies(
        &mut self,
        _conversation: ConversationId,
        _target_seq: i64,
    ) -> DomainResult<()> {
        unimplemented!("plan 3B task 4")
    }

    async fn entries_after(
        &mut self,
        _conversation: ConversationId,
        _after: i64,
        _up_to: i64,
        _targets_from: i64,
        _limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn count_incoming_messages(
        &mut self,
        _conversation: ConversationId,
        _user: UserId,
        _after: i64,
        _up_to: i64,
        _cap: u32,
    ) -> DomainResult<u32> {
        unimplemented!("plan 3B task 4")
    }

    async fn first_live_message(
        &mut self,
        _conversation: ConversationId,
        _after: i64,
        _up_to: i64,
        _not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        unimplemented!("plan 3B task 4")
    }

    async fn messages_before(
        &mut self,
        _conversation: ConversationId,
        _from: i64,
        _before: i64,
        _limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        unimplemented!("plan 3B task 4")
    }

    async fn get_cursor(
        &mut self,
        _device: DeviceId,
        _conversation: ConversationId,
    ) -> DomainResult<Option<i64>> {
        unimplemented!("plan 3B task 5")
    }

    async fn create_cursor_if_absent(
        &mut self,
        _device: DeviceId,
        _conversation: ConversationId,
        _seq: i64,
    ) -> DomainResult<()> {
        unimplemented!("plan 3B task 5")
    }

    async fn raise_cursor(
        &mut self,
        _device: DeviceId,
        _conversation: ConversationId,
        _seq: i64,
    ) -> DomainResult<bool> {
        unimplemented!("plan 3B task 5")
    }

    async fn delivered_seq(
        &mut self,
        _conversation: ConversationId,
        _user: UserId,
    ) -> DomainResult<i64> {
        unimplemented!("plan 3B task 5")
    }

    async fn member_positions(
        &mut self,
        _conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>> {
        unimplemented!("plan 3B task 5")
    }

    async fn sync_positions(
        &mut self,
        _user: UserId,
        _device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>> {
        unimplemented!("plan 3B task 5")
    }

    async fn group_positions(
        &mut self,
        _conversation: ConversationId,
        _last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>> {
        unimplemented!("plan 3B task 5")
    }

    async fn conversation_overviews(
        &mut self,
        _user: UserId,
        _changed_since: Option<DateTime<Utc>>,
        _unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>> {
        unimplemented!("plan 3B task 6")
    }
}
