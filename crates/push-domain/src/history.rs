use crate::clock::Clock;
use crate::conversations::{require_conversation, require_member};
use crate::error::DomainResult;
use crate::ids::{ConversationId, UserId};
use crate::model::Entry;
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::MAX_HISTORY_PAGE;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Up to `limit` entries before `before_seq` (or the newest ones), oldest first,
    /// limited to what `user` may see. `before_seq` is exclusive.
    pub async fn history(
        &self,
        user: UserId,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: u32,
    ) -> DomainResult<Vec<Entry>> {
        let limit = limit.clamp(1, MAX_HISTORY_PAGE);
        let mut tx = self.store.begin().await?;
        let found = require_conversation(&mut tx, conversation).await?;
        let member = require_member(&mut tx, conversation, user).await?;

        let end = member.visible_upper(found.last_seq) + 1;
        let before = before_seq.map_or(end, |seq| seq.min(end));
        let page = tx
            .entries_before(conversation, before, member.visible_from_seq, limit)
            .await?;
        tx.rollback().await?;
        Ok(page)
    }
}
