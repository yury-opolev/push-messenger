//! Agent connections: policies, refused requests, the agents' request log, waiting and
//! outgoing counts, and message counts for the owner view.

use chrono::TimeDelta;
use push_domain::calls::{CallHistoryRow, EndReason, ended_rows};
use push_domain::{
    AgentPolicy, AgentSettings, ClientMsgId, ContactState, Conversation, ConversationId,
    DomainError, Entry, EntryKind, InboundPolicy, Member, OutgoingAction, OutgoingError,
    OutgoingKind, OutgoingPolicy, OutgoingState, Store, Tx, UserId,
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

/// A waiting action of `agent`, created `minutes` after [`start`].
fn action(agent: UserId, kind: OutgoingKind, minutes: i64) -> OutgoingAction {
    OutgoingAction {
        action_id: UserId::new().0,
        agent,
        kind,
        target: None,
        conversation: None,
        title: None,
        members: vec![],
        state: OutgoingState::Waiting,
        error: None,
        created_at: start() + TimeDelta::minutes(minutes),
        resolved_at: None,
        used_at: None,
    }
}

async fn settings_default_until_set_and_then_replaced<B: Backend>(backend: &B) {
    let (store, [owner, agent, ..]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    let defaults = AgentSettings {
        inbound: InboundPolicy::OwnerOnly,
        requests: OutgoingPolicy::Ask,
        chats: OutgoingPolicy::Ask,
        groups: OutgoingPolicy::Off,
        calls: OutgoingPolicy::Off,
    };
    assert_eq!(AgentSettings::default(), defaults);
    assert_eq!(tx.agent_settings(agent).await, Ok(defaults));
    let first = AgentSettings {
        inbound: InboundPolicy::Contacts,
        requests: OutgoingPolicy::Allowed,
        ..defaults
    };
    let second = AgentSettings {
        inbound: InboundPolicy::OwnerAgents,
        requests: OutgoingPolicy::Off,
        chats: OutgoingPolicy::Allowed,
        groups: OutgoingPolicy::Ask,
        calls: OutgoingPolicy::Allowed,
    };
    tx.set_agent_settings(agent, &first, start()).await.unwrap();
    tx.set_agent_settings(agent, &second, start() + TimeDelta::minutes(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.agent_settings(agent).await, Ok(second));
    assert_eq!(tx.agent_settings(owner).await, Ok(defaults));
    tx.rollback().await.unwrap();

    // Only recorded agents have settings.
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.set_agent_settings(owner, &first, start()).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();

    for policy in [
        OutgoingPolicy::Allowed,
        OutgoingPolicy::Ask,
        OutgoingPolicy::Off,
    ] {
        assert_eq!(OutgoingPolicy::from_code(policy.code()), Some(policy));
    }
    for policy in [
        InboundPolicy::OwnerOnly,
        InboundPolicy::OwnerAgents,
        InboundPolicy::Contacts,
    ] {
        assert_eq!(InboundPolicy::from_code(policy.code()), Some(policy));
    }
    for kind in OutgoingKind::ALL {
        assert_eq!(OutgoingKind::from_code(kind.code()), Some(kind));
    }
    for state in OutgoingState::ALL {
        assert_eq!(OutgoingState::from_code(state.code()), Some(state));
    }
    for error in OutgoingError::ALL {
        assert_eq!(OutgoingError::from_code(error.code()), Some(error));
    }
    assert_eq!(OutgoingPolicy::from_code(0), None);
    assert_eq!(OutgoingKind::from_code(6), None);
}

async fn actions_are_found_again_listed_and_resolved_once<B: Backend>(backend: &B) {
    let (store, [owner, agent, alice, bob]) = people(backend).await;
    let group = ConversationId::new();
    let request = OutgoingAction {
        target: Some(alice),
        ..action(agent, OutgoingKind::Request, 0)
    };
    let create = OutgoingAction {
        title: Some("Team".into()),
        members: {
            let mut members = vec![alice, bob];
            members.sort();
            members
        },
        ..action(agent, OutgoingKind::Group, 1)
    };
    let invite = OutgoingAction {
        conversation: Some(group),
        members: vec![bob],
        ..action(agent, OutgoingKind::Invite, 2)
    };
    let mut tx = store.begin().await.unwrap();
    for action in [&request, &create, &invite] {
        tx.insert_outgoing(action).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    // The same action asked again is found; one that differs in any part is not.
    for (asked, found) in [
        (
            OutgoingAction {
                target: Some(alice),
                ..action(agent, OutgoingKind::Request, 9)
            },
            Some(&request),
        ),
        (
            OutgoingAction {
                target: Some(bob),
                ..action(agent, OutgoingKind::Request, 9)
            },
            None,
        ),
        (
            OutgoingAction {
                target: Some(alice),
                ..action(agent, OutgoingKind::Chat, 9)
            },
            None,
        ),
        (
            OutgoingAction {
                title: Some("Team".into()),
                members: create.members.clone(),
                ..action(agent, OutgoingKind::Group, 9)
            },
            Some(&create),
        ),
        (
            OutgoingAction {
                title: Some("Other".into()),
                members: create.members.clone(),
                ..action(agent, OutgoingKind::Group, 9)
            },
            None,
        ),
        (
            OutgoingAction {
                title: Some("Team".into()),
                members: vec![alice],
                ..action(agent, OutgoingKind::Group, 9)
            },
            None,
        ),
        (
            OutgoingAction {
                conversation: Some(group),
                members: vec![bob],
                ..action(agent, OutgoingKind::Invite, 9)
            },
            Some(&invite),
        ),
        (
            OutgoingAction {
                conversation: Some(ConversationId::new()),
                members: vec![bob],
                ..action(agent, OutgoingKind::Invite, 9)
            },
            None,
        ),
        (
            OutgoingAction {
                target: Some(alice),
                ..action(owner, OutgoingKind::Request, 9)
            },
            None,
        ),
    ] {
        assert_eq!(
            tx.find_waiting_outgoing(&asked).await,
            Ok(found.cloned()),
            "{asked:?}"
        );
    }
    assert_eq!(
        tx.outgoing(create.action_id).await,
        Ok(Some(create.clone()))
    );
    assert_eq!(tx.outgoing(UserId::new().0).await, Ok(None));
    assert_eq!(tx.waiting_outgoing_count(agent).await, Ok(3));
    assert_eq!(tx.waiting_outgoing_count(owner).await, Ok(0));
    assert_eq!(
        tx.list_outgoing(agent, None).await,
        Ok(vec![request.clone(), create.clone(), invite.clone()])
    );

    // Resolving moves it from the expected state only.
    let at = start() + TimeDelta::minutes(5);
    let chat = ConversationId::new();
    let failed = OutgoingAction {
        state: OutgoingState::Failed,
        error: Some(OutgoingError::NotContact),
        resolved_at: Some(at),
        ..request.clone()
    };
    assert_eq!(
        tx.resolve_outgoing(
            request.action_id,
            OutgoingState::Approved,
            OutgoingState::Failed,
            Some(OutgoingError::NotContact),
            None,
            at
        )
        .await,
        Ok(None)
    );
    assert_eq!(
        tx.resolve_outgoing(
            request.action_id,
            OutgoingState::Waiting,
            OutgoingState::Failed,
            Some(OutgoingError::NotContact),
            None,
            at
        )
        .await,
        Ok(Some(failed.clone()))
    );
    let approved = OutgoingAction {
        state: OutgoingState::Approved,
        conversation: Some(chat),
        resolved_at: Some(at),
        ..create.clone()
    };
    assert_eq!(
        tx.resolve_outgoing(
            create.action_id,
            OutgoingState::Waiting,
            OutgoingState::Approved,
            None,
            Some(chat),
            at
        )
        .await,
        Ok(Some(approved.clone()))
    );
    // An invite keeps its group when resolved without a conversation.
    let declined = tx
        .resolve_outgoing(
            invite.action_id,
            OutgoingState::Waiting,
            OutgoingState::Declined,
            None,
            None,
            at + TimeDelta::minutes(1),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(declined.conversation, Some(group));
    assert_eq!(
        tx.resolve_outgoing(
            invite.action_id,
            OutgoingState::Waiting,
            OutgoingState::Approved,
            None,
            None,
            at
        )
        .await,
        Ok(None)
    );
    assert_eq!(
        tx.resolve_outgoing(
            UserId::new().0,
            OutgoingState::Waiting,
            OutgoingState::Declined,
            None,
            None,
            at
        )
        .await,
        Ok(None)
    );
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.waiting_outgoing_count(agent).await, Ok(0));
    assert_eq!(tx.find_waiting_outgoing(&request).await, Ok(None));
    assert_eq!(tx.list_outgoing(agent, None).await, Ok(vec![]));
    assert_eq!(
        tx.list_outgoing(agent, Some(at)).await,
        Ok(vec![failed, approved, declined.clone()])
    );
    assert_eq!(
        tx.list_outgoing(agent, Some(at + TimeDelta::minutes(1)))
            .await,
        Ok(vec![declined])
    );
    tx.rollback().await.unwrap();

    // An action needs a known agent and a new id.
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_outgoing(&action(UserId::new(), OutgoingKind::Chat, 0))
            .await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_outgoing(&request).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
}

async fn call_grants_are_used_once<B: Backend>(backend: &B) {
    let (store, [_, agent, alice, bob]) = people(backend).await;
    let call = OutgoingAction {
        target: Some(alice),
        ..action(agent, OutgoingKind::Call, 0)
    };
    let approved_at = start() + TimeDelta::minutes(10);
    let mut tx = store.begin().await.unwrap();
    tx.insert_outgoing(&call).await.unwrap();
    // A waiting call is no grant.
    assert_eq!(
        tx.approved_call_grant(agent, alice, start()).await,
        Ok(None)
    );
    tx.resolve_outgoing(
        call.action_id,
        OutgoingState::Waiting,
        OutgoingState::Approved,
        None,
        None,
        approved_at,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.approved_call_grant(agent, alice, approved_at).await,
        Ok(Some(call.action_id))
    );
    assert_eq!(
        tx.approved_call_grant(agent, alice, approved_at + TimeDelta::seconds(1))
            .await,
        Ok(None)
    );
    assert_eq!(tx.approved_call_grant(agent, bob, start()).await, Ok(None));
    assert_eq!(
        tx.use_outgoing(call.action_id, approved_at + TimeDelta::minutes(1))
            .await,
        Ok(true)
    );
    assert_eq!(
        tx.use_outgoing(call.action_id, approved_at + TimeDelta::minutes(2))
            .await,
        Ok(false)
    );
    assert_eq!(
        tx.approved_call_grant(agent, alice, start()).await,
        Ok(None)
    );
    assert_eq!(
        tx.outgoing(call.action_id).await.unwrap().unwrap().used_at,
        Some(approved_at + TimeDelta::minutes(1))
    );
}

async fn waiting_actions_expire_and_resolved_ones_are_pruned<B: Backend>(backend: &B) {
    let (store, [owner, agent, alice, _]) = people(backend).await;
    let old = OutgoingAction {
        target: Some(alice),
        ..action(agent, OutgoingKind::Chat, 0)
    };
    let older = OutgoingAction {
        target: Some(owner),
        ..action(agent, OutgoingKind::Request, -5)
    };
    let fresh = OutgoingAction {
        target: Some(alice),
        ..action(agent, OutgoingKind::Call, 30)
    };
    let decided = OutgoingAction {
        target: Some(alice),
        ..action(agent, OutgoingKind::Request, -10)
    };
    let mut tx = store.begin().await.unwrap();
    for action in [&old, &older, &fresh, &decided] {
        tx.insert_outgoing(action).await.unwrap();
    }
    tx.resolve_outgoing(
        decided.action_id,
        OutgoingState::Waiting,
        OutgoingState::Declined,
        None,
        None,
        start(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let now = start() + TimeDelta::hours(1);
    let mut tx = store.begin().await.unwrap();
    let expired = |action: &OutgoingAction| OutgoingAction {
        state: OutgoingState::Expired,
        resolved_at: Some(now),
        ..action.clone()
    };
    assert_eq!(
        tx.expire_outgoing(start(), now, 1).await,
        Ok(vec![expired(&older)])
    );
    assert_eq!(
        tx.expire_outgoing(start(), now, 10).await,
        Ok(vec![expired(&old)])
    );
    assert_eq!(tx.expire_outgoing(start(), now, 10).await, Ok(vec![]));
    assert_eq!(tx.waiting_outgoing_count(agent).await, Ok(1));
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    // Only resolved actions resolved before the cutoff go.
    assert_eq!(tx.prune_outgoing(now).await, Ok(1));
    assert_eq!(tx.outgoing(decided.action_id).await, Ok(None));
    assert_eq!(tx.prune_outgoing(now + TimeDelta::seconds(1)).await, Ok(2));
    assert_eq!(
        tx.list_outgoing(agent, Some(start())).await,
        Ok(vec![fresh])
    );
}

async fn the_action_log_sums_per_kind_and_prunes<B: Backend>(backend: &B) {
    let (store, [owner, agent, ..]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    for (minutes, kind, count) in [
        (0, OutgoingKind::Invite, 3),
        (30, OutgoingKind::Invite, 2),
        (40, OutgoingKind::Chat, 1),
        (90, OutgoingKind::Invite, 1),
    ] {
        tx.log_agent_action(agent, kind, count, start() + TimeDelta::minutes(minutes))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.agent_actions_since(agent, OutgoingKind::Invite, start())
            .await,
        Ok((6, Some(start())))
    );
    assert_eq!(
        tx.agent_actions_since(
            agent,
            OutgoingKind::Invite,
            start() + TimeDelta::minutes(30)
        )
        .await,
        Ok((3, Some(start() + TimeDelta::minutes(30))))
    );
    assert_eq!(
        tx.agent_actions_since(agent, OutgoingKind::Chat, start())
            .await,
        Ok((1, Some(start() + TimeDelta::minutes(40))))
    );
    assert_eq!(
        tx.agent_actions_since(agent, OutgoingKind::Group, start())
            .await,
        Ok((0, None))
    );
    assert_eq!(
        tx.agent_actions_since(owner, OutgoingKind::Invite, start())
            .await,
        Ok((0, None))
    );
    assert_eq!(
        tx.prune_agent_actions(start() + TimeDelta::minutes(30))
            .await,
        Ok(1)
    );
    assert_eq!(
        tx.agent_actions_since(agent, OutgoingKind::Invite, start())
            .await,
        Ok((3, Some(start() + TimeDelta::minutes(30))))
    );
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.log_agent_action(UserId::new(), OutgoingKind::Chat, 1, start())
            .await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
}

async fn forgetting_an_agent_drops_its_settings_actions_and_log<B: Backend>(backend: &B) {
    let (store, [owner, agent, alice, _]) = people(backend).await;
    let mut tx = store.begin().await.unwrap();
    tx.record_agent_owner(alice, owner).await.unwrap();
    let settings = AgentSettings {
        chats: OutgoingPolicy::Allowed,
        ..AgentSettings::default()
    };
    for user in [agent, alice] {
        tx.set_agent_settings(user, &settings, start())
            .await
            .unwrap();
        tx.insert_outgoing(&action(user, OutgoingKind::Chat, 0))
            .await
            .unwrap();
        let resolved = action(user, OutgoingKind::Request, 0);
        tx.insert_outgoing(&resolved).await.unwrap();
        tx.resolve_outgoing(
            resolved.action_id,
            OutgoingState::Waiting,
            OutgoingState::Declined,
            None,
            None,
            start(),
        )
        .await
        .unwrap();
        tx.log_agent_action(user, OutgoingKind::Chat, 1, start())
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    tx.forget_agent(agent).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.agent_settings(agent).await, Ok(AgentSettings::default()));
    assert_eq!(tx.list_outgoing(agent, Some(start())).await, Ok(vec![]));
    assert_eq!(
        tx.agent_actions_since(agent, OutgoingKind::Chat, start())
            .await,
        Ok((0, None))
    );
    // Another agent keeps everything.
    assert_eq!(tx.agent_settings(alice).await, Ok(settings));
    assert_eq!(
        tx.list_outgoing(alice, Some(start())).await.unwrap().len(),
        2
    );
    assert_eq!(
        tx.agent_actions_since(alice, OutgoingKind::Chat, start())
            .await,
        Ok((1, Some(start())))
    );
}

async fn started_by_agent_is_stored_with_conversations_and_calls<B: Backend>(backend: &B) {
    let (store, [_, agent, alice, _]) = people(backend).await;
    let marked = Conversation {
        started_by_agent: true,
        ..conversation()
    };
    let plain = conversation();
    let rows: Vec<CallHistoryRow> = ended_rows(
        UserId::new().0,
        agent,
        alice,
        start().timestamp_millis(),
        None,
        start().timestamp_millis() + 30_000,
        EndReason::Missed,
    )
    .into_iter()
    .map(|row| CallHistoryRow {
        started_by_agent: true,
        ..row
    })
    .collect();
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(marked.clone()).await.unwrap();
    tx.insert_conversation(plain.clone()).await.unwrap();
    tx.insert_call_history(&rows).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.get_conversation(marked.id).await,
        Ok(Some(marked.clone()))
    );
    assert_eq!(tx.get_conversation(plain.id).await, Ok(Some(plain)));
    tx.insert_member(member(marked.id, alice)).await.unwrap();
    let overviews = tx.conversation_overviews(alice, None, 10).await.unwrap();
    assert_eq!(overviews.len(), 1);
    assert!(overviews[0].conversation.started_by_agent);
    assert_eq!(
        tx.call_history(agent, None, None, 10).await,
        Ok(vec![rows[0].clone()])
    );
    assert_eq!(
        tx.call_history(alice, None, None, 10).await,
        Ok(vec![rows[1].clone()])
    );
}

pub(super) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_policy_is_absent_until_set_and_then_replaced,
        refusals_keep_their_first_release_and_come_due_in_order,
        the_request_log_counts_windows_and_prunes,
        waiting_and_outgoing_requests_are_counted,
        message_counts_follow_the_visible_range_and_skip_deleted,
        deleted_users_are_known,
        settings_default_until_set_and_then_replaced,
        actions_are_found_again_listed_and_resolved_once,
        call_grants_are_used_once,
        waiting_actions_expire_and_resolved_ones_are_pruned,
        the_action_log_sums_per_kind_and_prunes,
        forgetting_an_agent_drops_its_settings_actions_and_log,
        started_by_agent_is_stored_with_conversations_and_calls,
    ]
}
