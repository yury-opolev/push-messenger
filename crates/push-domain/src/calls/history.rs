//! Call history (spec §9): one row per participant of an ended call, kept apart from
//! conversation history. Only the row's own user reads or hides it.

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use super::machine::{CallDirection, EndReason};
use crate::clock::Clock;
use crate::error::DomainResult;
use crate::ids::UserId;
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};

/// The most rows one history page returns.
pub const MAX_CALL_HISTORY_PAGE: u32 = 100;
/// The rows a history page returns when the caller does not say.
pub const DEFAULT_CALL_HISTORY_PAGE: u32 = 50;

/// One call as one of its participants sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallHistoryRow {
    pub call_id: Uuid,
    /// Whose history the row belongs to.
    pub user: UserId,
    /// The other participant.
    pub peer: UserId,
    pub direction: CallDirection,
    pub started_at: DateTime<Utc>,
    /// Calls that were answered only.
    pub answered_at: Option<DateTime<Utc>>,
    pub ended_at: DateTime<Utc>,
    /// Whole seconds from the answer to the end; 0 for calls never answered.
    pub duration_s: u32,
    pub outcome: EndReason,
}

impl CallHistoryRow {
    /// The row's position in its user's history.
    pub fn cursor(&self) -> HistoryCursor {
        HistoryCursor {
            ended_at: self.ended_at,
            call_id: self.call_id,
        }
    }
}

/// A position in a user's history: rows are ordered by `(ended_at, call_id)`, newest
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistoryCursor {
    pub ended_at: DateTime<Utc>,
    pub call_id: Uuid,
}

/// The history rows of a call that ended: the caller's always, the callee's unless the
/// call was `Unreachable` (the callee never saw it). Times are Redis-clock milliseconds;
/// `answered_ms` is set for calls that were answered.
pub fn ended_rows(
    call_id: Uuid,
    caller: UserId,
    callee: UserId,
    created_ms: i64,
    answered_ms: Option<i64>,
    ended_ms: i64,
    outcome: EndReason,
) -> Vec<CallHistoryRow> {
    let duration_s = answered_ms.map_or(0, |answered| {
        u32::try_from((ended_ms - answered).max(0) / 1000).unwrap_or(u32::MAX)
    });
    let row = |user, peer, direction| CallHistoryRow {
        call_id,
        user,
        peer,
        direction,
        started_at: from_millis(created_ms),
        answered_at: answered_ms.map(from_millis),
        ended_at: from_millis(ended_ms),
        duration_s,
        outcome,
    };
    let mut rows = vec![row(caller, callee, CallDirection::Out)];
    if outcome != EndReason::Unreachable {
        rows.push(row(callee, caller, CallDirection::In));
    }
    rows
}

fn from_millis(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Stores the history rows of an ended call. Idempotent: a row already stored (the
    /// same user and call) is kept as it is, hidden or not.
    pub async fn record_call(&self, rows: &[CallHistoryRow]) -> DomainResult<()> {
        let mut tx = self.store.begin().await?;
        tx.insert_call_history(rows).await?;
        tx.commit().await
    }

    /// `user`'s calls that they did not hide, newest first: those before `before` and
    /// after `since` (each optional), at most `limit` (0: [`DEFAULT_CALL_HISTORY_PAGE`];
    /// at most [`MAX_CALL_HISTORY_PAGE`]).
    pub async fn call_history(
        &self,
        user: UserId,
        before: Option<HistoryCursor>,
        since: Option<HistoryCursor>,
        limit: u32,
    ) -> DomainResult<Vec<CallHistoryRow>> {
        let limit = match limit {
            0 => DEFAULT_CALL_HISTORY_PAGE,
            limit => limit.min(MAX_CALL_HISTORY_PAGE),
        };
        let mut tx = self.store.begin().await?;
        let rows = tx.call_history(user, before, since, limit).await;
        tx.rollback().await?;
        rows
    }

    /// Hides `call` from `user`'s history only; the other participant's row is untouched.
    /// Returns whether a visible row was hidden (false for an unknown call, another user's
    /// call or one already hidden).
    pub async fn hide_call(&self, user: UserId, call: Uuid) -> DomainResult<bool> {
        let mut tx = self.store.begin().await?;
        let hidden = tx.hide_call(user, call, self.clock.now()).await?;
        tx.commit().await?;
        Ok(hidden)
    }

    /// Deletes at most `batch` history rows of calls that ended more than `keep` ago, hidden
    /// or not. Returns how many were deleted.
    pub async fn purge_call_history(&self, keep: TimeDelta, batch: u32) -> DomainResult<u64> {
        let cutoff = self.clock.now() - keep;
        let mut tx = self.store.begin().await?;
        let deleted = tx.delete_call_history_before(cutoff, batch).await?;
        tx.commit().await?;
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answered_call_gives_both_sides_a_row_with_its_duration() {
        let (call, alice, bob) = (Uuid::now_v7(), UserId::new(), UserId::new());
        let rows = ended_rows(
            call,
            alice,
            bob,
            1_000,
            Some(4_000),
            65_999,
            EndReason::Completed,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].user, rows[0].peer, rows[0].direction),
            (alice, bob, CallDirection::Out)
        );
        assert_eq!(
            (rows[1].user, rows[1].peer, rows[1].direction),
            (bob, alice, CallDirection::In)
        );
        for row in &rows {
            assert_eq!(row.duration_s, 61);
            assert_eq!(row.started_at, from_millis(1_000));
            assert_eq!(row.answered_at, Some(from_millis(4_000)));
            assert_eq!(row.ended_at, from_millis(65_999));
            assert_eq!(row.outcome, EndReason::Completed);
        }
    }

    #[test]
    fn an_unanswered_call_has_no_duration_and_unreachable_only_a_caller_row() {
        let (call, alice, bob) = (Uuid::now_v7(), UserId::new(), UserId::new());
        let missed = ended_rows(call, alice, bob, 1_000, None, 46_000, EndReason::Missed);
        assert_eq!(missed.len(), 2);
        assert!(
            missed
                .iter()
                .all(|row| row.duration_s == 0 && row.answered_at.is_none())
        );
        let unreachable = ended_rows(call, alice, bob, 1_000, None, 1_000, EndReason::Unreachable);
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0].user, alice);
        assert_eq!(unreachable[0].direction, CallDirection::Out);
    }
}
