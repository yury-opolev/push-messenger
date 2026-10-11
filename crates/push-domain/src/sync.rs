use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, DeviceId};
use crate::model::Entry;
use crate::notify::Notifier;
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
            if room == 0 {
                more = true;
                break;
            }
            let page = tx
                .entries_after(conversation.id, position, upper, room as u32)
                .await?;
            if page.last().map_or(position, |entry| entry.seq) < upper {
                more = true;
            }
            entries.extend(page);
        }
        tx.rollback().await?;
        Ok(SyncBatch { entries, more })
    }

    /// Records that the device holds everything up to `seq`. Returns whether the
    /// position moved; it never moves backwards.
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
        let moved = tx.raise_cursor(device, conversation, seq).await?;
        tx.commit().await?;
        Ok(moved)
    }
}
