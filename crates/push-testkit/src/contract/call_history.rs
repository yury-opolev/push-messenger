//! Call history (voice calls spec §9): rows per participant, read and hidden per user,
//! paged newest first, deleted by age.

use chrono::{DateTime, TimeDelta, Utc};
use push_domain::calls::{CallDirection, CallHistoryRow, EndReason, HistoryCursor, ended_rows};
use push_domain::{DomainError, Store, Tx, UserId};

use super::{add_users, start};
use crate::{Backend, Case, case_list};

/// The two rows of a missed call from `caller` to `callee` that ended `minutes` after
/// [`start`].
fn missed(caller: UserId, callee: UserId, minutes: i64) -> Vec<CallHistoryRow> {
    let ended = (start() + TimeDelta::minutes(minutes)).timestamp_millis();
    ended_rows(
        UserId::new().0,
        caller,
        callee,
        ended - 45_000,
        None,
        ended,
        EndReason::Missed,
    )
}

async fn history<S: Store>(
    store: &S,
    user: UserId,
    before: Option<HistoryCursor>,
    since: Option<HistoryCursor>,
    limit: u32,
) -> Vec<CallHistoryRow> {
    let mut tx = store.begin().await.unwrap();
    let rows = tx.call_history(user, before, since, limit).await.unwrap();
    tx.rollback().await.unwrap();
    rows
}

async fn insert<S: Store>(store: &S, rows: &[CallHistoryRow]) {
    let mut tx = store.begin().await.unwrap();
    tx.insert_call_history(rows).await.unwrap();
    tx.commit().await.unwrap();
}

/// Each participant reads only their own row; hiding one changes nothing for the other
/// participant or for anyone else, and recording the call again does not show it again.
async fn history_is_private_and_hides_per_side<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
    add_users(&store, &[alice, bob, carol]).await;
    let ended = start() + TimeDelta::minutes(5);
    let call = UserId::new().0;
    let rows = ended_rows(
        call,
        alice,
        bob,
        ended.timestamp_millis() - 70_000,
        Some(ended.timestamp_millis() - 61_500),
        ended.timestamp_millis(),
        EndReason::Completed,
    );
    insert(&store, &rows).await;

    let mine = history(&store, alice, None, None, 50).await;
    assert_eq!(mine, vec![rows[0].clone()]);
    assert_eq!(mine[0].direction, CallDirection::Out);
    assert_eq!(mine[0].peer, bob);
    assert_eq!(mine[0].duration_s, 61);
    assert_eq!(mine[0].ended_at, ended);
    let theirs = history(&store, bob, None, None, 50).await;
    assert_eq!(theirs, vec![rows[1].clone()]);
    assert_eq!(theirs[0].direction, CallDirection::In);
    assert!(history(&store, carol, None, None, 50).await.is_empty());

    let mut tx = store.begin().await.unwrap();
    // Not carol's call: nothing to hide, and nobody's row changes.
    assert_eq!(tx.hide_call(carol, call, ended).await, Ok(false));
    assert_eq!(tx.hide_call(alice, UserId::new().0, ended).await, Ok(false));
    assert_eq!(tx.hide_call(alice, call, ended).await, Ok(true));
    assert_eq!(tx.hide_call(alice, call, ended).await, Ok(false));
    tx.commit().await.unwrap();
    assert!(history(&store, alice, None, None, 50).await.is_empty());
    assert_eq!(history(&store, bob, None, None, 50).await, theirs);

    // The end recorded again keeps alice's row hidden.
    insert(&store, &rows).await;
    assert!(history(&store, alice, None, None, 50).await.is_empty());
    assert_eq!(history(&store, bob, None, None, 50).await, theirs);
}

/// Pages are newest first by (ended_at, call_id); `before` and `since` are exclusive and
/// combine, so paging never repeats or skips a row, even with equal end times.
async fn pages_are_newest_first_and_stable<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut all = Vec::new();
    for minutes in [1, 2, 2, 2, 3, 4] {
        let rows = missed(alice, bob, minutes);
        insert(&store, &rows).await;
        all.push(rows[0].clone());
    }
    all.sort_by_key(|row| std::cmp::Reverse(row.cursor()));

    assert_eq!(history(&store, alice, None, None, 50).await, all);
    let mut paged = Vec::new();
    let mut before = None;
    loop {
        let page = history(&store, alice, before, None, 4).await;
        assert!(page.len() <= 4);
        let Some(last) = page.last() else {
            break;
        };
        before = Some(last.cursor());
        paged.extend(page);
    }
    assert_eq!(paged, all);

    let since = all[3].cursor();
    assert_eq!(
        history(&store, alice, None, Some(since), 50).await,
        all[..3]
    );
    assert_eq!(
        history(&store, alice, Some(all[0].cursor()), Some(since), 50).await,
        all[1..3]
    );
    assert_eq!(history(&store, alice, None, Some(since), 2).await, all[..2]);
    assert!(
        history(&store, alice, None, Some(all[0].cursor()), 50)
            .await
            .is_empty()
    );
}

/// The purge deletes rows that ended before the cutoff, hidden or not, in batches, and
/// keeps the newer ones.
async fn retention_deletes_only_old_rows<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let old = [missed(alice, bob, 1), missed(bob, alice, 2)];
    let new = missed(alice, bob, 10);
    for rows in old.iter().chain([&new]) {
        insert(&store, rows).await;
    }
    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.hide_call(alice, old[0][0].call_id, start()).await,
        Ok(true)
    );
    tx.commit().await.unwrap();

    let cutoff: DateTime<Utc> = start() + TimeDelta::minutes(5);
    let mut deleted = 0;
    loop {
        let mut tx = store.begin().await.unwrap();
        let batch = tx.delete_call_history_before(cutoff, 3).await.unwrap();
        tx.commit().await.unwrap();
        assert!(batch <= 3);
        if batch == 0 {
            break;
        }
        deleted += batch;
    }
    assert_eq!(deleted, 4);
    assert_eq!(
        history(&store, alice, None, None, 50).await,
        vec![new[0].clone()]
    );
    assert_eq!(
        history(&store, bob, None, None, 50).await,
        vec![new[1].clone()]
    );
}

async fn a_row_of_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_call_history(&missed(alice, UserId::new(), 1))
            .await,
        Err(DomainError::Store(_))
    ));
    drop(tx);
    assert!(history(&store, alice, None, None, 50).await.is_empty());
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        history_is_private_and_hides_per_side,
        pages_are_newest_first_and_stable,
        retention_deletes_only_old_rows,
        a_row_of_an_unknown_user_is_refused,
    ]
}
