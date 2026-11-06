//! Agent connections: policies, refused requests, the agents' request log, waiting and
//! outgoing counts, and message counts for the owner view.

use chrono::TimeDelta;
use push_domain::{
    AgentPolicy, ClientMsgId, ContactState, Conversation, DomainError, Entry, EntryKind, Member,
    Store, Tx, UserId,
};

use super::{add_users, conversation, entry, member, start};
use crate::{Backend, Case, case_list};

/// A store with an owner, its agent and two more users, all known.
async fn people<B: Backend>(backend: &B) -> (B::Store, [UserId; 4]) {
    let store = backend.store().await;
    let users = [UserId::new(), UserId::new(), UserId::new(), UserId::new()];
    add_users(&store, &users).await;
    let mut tx = store.begin().await.unwrap();
    tx.record_agent_owner(users[1], users[0]).await.unwrap();
    tx.commit().await.unwrap();
    (store, users)
}

async fn a_policy_is_absent_until_set_and_then_replaced<B: Backend>(backend: &B) {
    let (store, [owner, agent, ..]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.agent_policy(agent).await, Ok(None));
    tx.set_agent_policy(agent, AgentPolicy::AutoAny, start())
        .await
        .unwrap();
    tx.set_agent_policy(
        agent,
        AgentPolicy::RefuseAll,
        start() + TimeDelta::minutes(1),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.agent_policy(agent).await,
        Ok(Some(AgentPolicy::RefuseAll))
    );
    assert_eq!(tx.agent_policy(owner).await, Ok(None));
    tx.rollback().await.unwrap();

    // Only recorded agents have a policy.
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.set_agent_policy(owner, AgentPolicy::Ask, start()).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();

    for policy in [
        AgentPolicy::Ask,
        AgentPolicy::AutoOwn,
        AgentPolicy::AutoOwnerContacts,
        AgentPolicy::AutoAny,
        AgentPolicy::RefuseAll,
    ] {
        assert_eq!(AgentPolicy::from_code(policy.code()), Some(policy));
    }
    assert_eq!(AgentPolicy::from_code(0), None);
}

async fn refusals_keep_their_first_release_and_come_due_in_order<B: Backend>(backend: &B) {
    let (store, [_, agent, alice, bob]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    let early = start() + TimeDelta::minutes(10);
    let late = start() + TimeDelta::minutes(20);
    assert_eq!(tx.put_refusal(bob, agent, late).await, Ok(true));
    assert_eq!(tx.put_refusal(alice, agent, early).await, Ok(true));
    // A repeated refusal keeps the first release time.
    assert_eq!(
        tx.put_refusal(alice, agent, start() + TimeDelta::hours(5))
            .await,
        Ok(false)
    );
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.due_refusals(start(), 10).await, Ok(vec![]));
    assert_eq!(tx.due_refusals(early, 10).await, Ok(vec![(alice, agent)]));
    assert_eq!(
        tx.due_refusals(late, 10).await,
        Ok(vec![(alice, agent), (bob, agent)])
    );
    assert_eq!(tx.due_refusals(late, 1).await, Ok(vec![(alice, agent)]));
    assert_eq!(tx.delete_refusal(alice, agent).await, Ok(true));
    assert_eq!(tx.delete_refusal(alice, agent).await, Ok(false));
    assert_eq!(tx.due_refusals(late, 10).await, Ok(vec![(bob, agent)]));
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.put_refusal(UserId::new(), agent, late).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
}

async fn the_request_log_counts_windows_and_prunes<B: Backend>(backend: &B) {
    let (store, [owner, agent, alice, bob]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    for (minutes, peer) in [(0, alice), (30, bob), (90, UserId::new())] {
        tx.log_agent_request(agent, peer, start() + TimeDelta::minutes(minutes))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.agent_requests_since(agent, start()).await,
        Ok((3, Some(start())))
    );
    assert_eq!(
        tx.agent_requests_since(agent, start() + TimeDelta::minutes(30))
            .await,
        Ok((2, Some(start() + TimeDelta::minutes(30))))
    );
    assert_eq!(tx.agent_requests_since(owner, start()).await, Ok((0, None)));
    assert_eq!(
        tx.prune_agent_requests(start() + TimeDelta::minutes(30))
            .await,
        Ok(1)
    );
    assert_eq!(
        tx.agent_requests_since(agent, start()).await,
        Ok((2, Some(start() + TimeDelta::minutes(30))))
    );
    tx.set_agent_policy(agent, AgentPolicy::AutoAny, start())
        .await
        .unwrap();
    // Deleting an agent forgets its policy and its log.
    tx.forget_agent(agent).await.unwrap();
    assert_eq!(tx.agent_requests_since(agent, start()).await, Ok((0, None)));
    assert_eq!(tx.agent_policy(agent).await, Ok(None));
    tx.forget_agent(agent).await.unwrap();
}

async fn waiting_and_outgoing_requests_are_counted<B: Backend>(backend: &B) {
    let (store, [owner, agent, alice, bob]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    // Alice asks the agent (waiting for its owner) and Bob (a person).
    tx.put_contact(alice, agent, ContactState::RequestedOut)
        .await
        .unwrap();
    tx.put_contact(agent, alice, ContactState::RequestedIn)
        .await
        .unwrap();
    tx.put_contact(alice, bob, ContactState::RequestedOut)
        .await
        .unwrap();
    tx.put_contact(bob, alice, ContactState::RequestedIn)
        .await
        .unwrap();
    tx.put_contact(agent, owner, ContactState::Accepted)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.outgoing_counts(alice).await, Ok((2, 1)));
    assert_eq!(tx.outgoing_counts(bob).await, Ok((0, 0)));
    assert_eq!(tx.waiting_in_count(agent).await, Ok(1));
    assert_eq!(tx.waiting_in_count(bob).await, Ok(1));
    let waiting = tx.waiting_requests(agent).await.unwrap();
    assert_eq!(
        waiting.iter().map(|(user, _)| *user).collect::<Vec<_>>(),
        [alice]
    );
    tx.rollback().await.unwrap();

    // The agent's own request to Bob waits at no owner: Bob is a person.
    let mut tx = store.begin().await.unwrap();
    tx.put_contact(agent, bob, ContactState::RequestedOut)
        .await
        .unwrap();
    assert_eq!(tx.outgoing_counts(agent).await, Ok((1, 0)));
}

async fn message_counts_follow_the_visible_range_and_skip_deleted<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (agent, peer) = (UserId::new(), UserId::new());
    add_users(&store, &[agent, peer]).await;
    let chat = Conversation {
        last_seq: 6,
        ..conversation()
    };
    let empty = conversation();
    let id = chat.id;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(chat).await.unwrap();
    tx.insert_conversation(empty.clone()).await.unwrap();
    // The agent sees from seq 2.
    tx.insert_member(Member {
        visible_from_seq: 2,
        ..member(id, agent)
    })
    .await
    .unwrap();
    tx.insert_member(member(id, peer)).await.unwrap();
    tx.insert_member(member(empty.id, agent)).await.unwrap();
    for seq in 1..=6 {
        let entry: Entry = match seq {
            4 => Entry {
                deleted_at: Some(start()),
                body: None,
                ..entry(id, seq, ClientMsgId::new())
            },
            5 => Entry {
                kind: EntryKind::Edit,
                target_seq: Some(2),
                ..entry(id, seq, ClientMsgId::new())
            },
            _ => entry(id, seq, ClientMsgId::new()),
        };
        tx.insert_entry(entry).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    // Seqs 2, 3 and 6: seq 1 is before the agent's range, 4 is deleted, 5 is an edit.
    assert_eq!(tx.message_counts(agent).await, Ok(vec![(id, 3)]));
    assert_eq!(tx.message_counts(peer).await, Ok(vec![(id, 4)]));
    assert_eq!(tx.message_counts(UserId::new()).await, Ok(vec![]));
}

async fn deleted_users_are_known<B: Backend>(backend: &B) {
    let (store, [_, agent, alice, _]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    tx.mark_user_deleted(agent, start()).await.unwrap();
    assert_eq!(tx.is_user_deleted(agent).await, Ok(true));
    assert_eq!(tx.is_user_deleted(alice).await, Ok(false));
    assert_eq!(tx.is_user_deleted(UserId::new()).await, Ok(false));
}

pub(super) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_policy_is_absent_until_set_and_then_replaced,
        refusals_keep_their_first_release_and_come_due_in_order,
        the_request_log_counts_windows_and_prunes,
        waiting_and_outgoing_requests_are_counted,
        message_counts_follow_the_visible_range_and_skip_deleted,
        deleted_users_are_known,
    ]
}
