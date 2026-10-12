use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, DeviceId};
use crate::model::Entry;
use crate::notify::Notifier;
use crate::read::{direct_receipt, group_positions, group_receipt};
use crate::service::{Messaging, require_device};
use crate::store::{Store, Tx};
use crate::validation::MAX_SYNC_BATCH;

/// Entries a device has not acknowledged yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncBatch {
    /// Oldest first within each conversation.
    pub entries: Vec<Entry>,
    /// More entries are waiting beyond this batch.
    pub more: bool,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Everything after the device's delivery positions, within each membership's
    /// visible range, up to `limit` entries.
    ///
    /// Edits and deletes of messages below the member's visible range are never delivered
    /// (spec §4.6). If such entries end a conversation, the device's position stays before
    /// them until it acks a later entry; nothing is lost, as they are never delivered.
    ///
    /// Sync does not advance anything: the position moves only when the device acks.
    /// Calling sync again without acking returns the same entries.
    pub async fn sync(&self, device: DeviceId, limit: u32) -> DomainResult<SyncBatch> {
        let limit = limit.clamp(1, MAX_SYNC_BATCH) as usize;
        let mut tx = self.store.begin().await?;
        let user = require_device(&mut tx, device).await?;
        let mut memberships = tx.memberships(user).await?;
        memberships.sort_by_key(|(conversation, _)| conversation.id);

        let mut entries = Vec::new();
        let mut more = false;
        for (conversation, member) in memberships {
            let floor = member.visible_from_seq - 1;
            let position = tx
                .get_cursor(device, conversation.id)
                .await?
                .unwrap_or(floor)
                .max(floor);
            let upper = member.visible_upper(conversation.last_seq);
            if position >= upper {
                continue;
            }
            let room = limit - entries.len();
            // One entry beyond the room tells whether more is waiting.
            let mut page = tx
                .entries_after(
                    conversation.id,
                    position,
                    upper,
                    member.visible_from_seq,
                    room as u32 + 1,
                )
                .await?;
            if page.len() > room {
                page.truncate(room);
                entries.extend(page);
                more = true;
                break;
            }
            entries.extend(page);
        }
        tx.rollback().await?;
        Ok(SyncBatch { entries, more })
    }

    /// Records that the device holds everything up to `seq`. Returns whether the
    /// position moved; it never moves backwards. If the user's delivered position moved
    /// over a message from the other member of a direct conversation, they get a receipt;
    /// if a group's group-wide positions moved, its members get them.
    pub async fn ack(
        &self,
        device: DeviceId,
        conversation: ConversationId,
        seq: i64,
    ) -> DomainResult<bool> {
        let mut tx = self.store.begin().await?;
        let user = require_device(&mut tx, device).await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;
        if seq < 0 || seq > member.visible_upper(found.last_seq) {
            return Err(DomainError::SeqOutOfRange);
        }
        if seq < member.visible_from_seq - 1 {
            return Ok(false);
        }
        let group_before = group_positions(&mut tx, &found, found.last_seq).await?;
        // Count only from the member's floor: a rejoining member's old positions lie below
        // it.
        let delivered_before = tx
            .delivered_seq(conversation, user)
            .await?
            .max(member.visible_from_seq - 1);
        let moved = tx.raise_cursor(device, conversation, seq).await?;
        let receipt = direct_receipt(
            &mut tx,
            &found,
            user,
            delivered_before,
            delivered_before.max(seq),
        )
        .await?;
        let group = group_receipt(&mut tx, &found, found.last_seq, group_before).await?;
        tx.commit().await?;
        self.publish(receipt.into_iter().chain(group).collect())
            .await;
        Ok(moved)
    }
}
