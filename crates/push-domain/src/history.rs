use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::DomainResult;
use crate::ids::{ConversationId, UserId};
use crate::model::Entry;
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::MAX_HISTORY_PAGE;

/// One page of history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    /// Messages and system entries in their current state, oldest first. Never edit or
    /// delete entries: their effect is already applied to the messages.
    pub entries: Vec<Entry>,
    /// Older entries visible to the caller exist before this page.
    pub has_more: bool,
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Up to `limit` messages before `before_seq` (or the newest ones), limited to what
    /// `user` may see. `before_seq` is exclusive.
    pub async fn history(
        &self,
        user: UserId,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> DomainResult<HistoryPage> {
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;

        let end = member.visible_upper(found.last_seq) + 1;
        let before = before_seq.map_or(end, |seq| seq.min(end));
        // One entry beyond the page tells whether older ones exist.
        let mut entries = tx
            .messages_before(conversation, member.visible_from_seq, before, limit + 1)
            .await?;
        tx.rollback().await?;
        let has_more = entries.len() > limit as usize;
        if has_more {
            entries.remove(0);
        }
        Ok(HistoryPage { entries, has_more })
    }
}
