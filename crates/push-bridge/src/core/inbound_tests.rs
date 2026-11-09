//! The inbound policy and agent turns (spec 2026-11-09 proactive agents §4, §7.2) with the
//! echo backend: who besides the owner reaches it, and how it answers agents and people.

use push_sdk::{ConnectionState, ContactState, ConversationId, Event, InboundPolicy};

use super::tests::{Harness, settle};
use crate::messenger::{Call, param, server_error, template};

/// The notice the test messenger's agent and owner give.
const NOTICE: &str =
    "Chats with Coda on Devbox are visible to its owner, Test Owner (@test-owner).";

/// The echo agent, its settings answering `inbound` (`None`: the default).
async fn started(inbound: Option<InboundPolicy>) -> Harness {
    let mut h = Harness::echo(300);
    if let Some(inbound) = inbound {
        h.messenger.set_inbound(inbound);
    }
    h.start();
    settle().await;
    h
}

/// A person known by `handle` who is the agent's accepted contact.
fn person(h: &Harness, handle: &str) -> push_sdk::UserId {
    let user = h.messenger.add_user(handle, false);
    h.messenger.set_contact(user, ContactState::Accepted);
    user
}

/// An agent of the same owner, an accepted contact.
fn sibling(h: &Harness, handle: &str) -> push_sdk::UserId {
    let user = h.messenger.add_agent(handle, h.messenger.owner_id());
    h.messenger.set_contact(user, ContactState::Accepted);
    user
}

/// An agent of another owner, an accepted contact.
fn foreign_agent(h: &Harness, handle: &str) -> push_sdk::UserId {
    let user = h.messenger.add_user(handle, true);
    h.messenger.set_contact(user, ContactState::Accepted);
    user
}

/// The rich replies' texts in `conversation`, in order.
fn replies_to(h: &Harness, conversation: ConversationId) -> Vec<String> {
    h.messenger
        .calls()
        .iter()
        .filter_map(|call| match call {
            Call::SendRich {
                conversation: to,
                body,
                ..
            } if *to == conversation && template(body).starts_with("agent.reply@") => {
                param(body, "text")
            }
            _ => None,
        })
        .collect()
}

/// The plain texts sent in `conversation`, in order.
fn plain_to(h: &Harness, conversation: ConversationId) -> Vec<String> {
    h.messenger
        .texts()
        .into_iter()
        .filter(|(to, _)| *to == conversation)
        .map(|(_, text)| text)
        .collect()
}

/// Waits for the private notice in `conversation`.
async fn refused(h: &Harness, conversation: ConversationId) {
    h.messenger
        .wait_until("private", |_| {
            plain_to(h, conversation).contains(&super::PRIVATE.to_owned())
        })
        .await;
}

/// Nothing reached the backend from `conversation`: no reply there and no activity.
fn untouched(h: &Harness, conversation: ConversationId) {
    assert!(replies_to(h, conversation).is_empty());
    assert_eq!(plain_to(h, conversation), [super::PRIVATE]);
    assert!(!h.messenger.calls().iter().any(|call| matches!(call,
        Call::Activity { conversation: to, .. } if *to == conversation)));
}

#[tokio::test(start_paused = true)]
async fn owner_only_keeps_every_contact_out() {
    let h = started(None).await;
    let (alice, own) = (person(&h, "alice"), sibling(&h, "own"));
    let (with_alice, with_own) = (
        h.messenger.conversation_with(alice),
        h.messenger.conversation_with(own),
    );
    h.messenger.says(alice, "hi");
    h.messenger.says(own, "hi");
    refused(&h, with_alice).await;
    refused(&h, with_own).await;
    settle().await;
    untouched(&h, with_alice);
    untouched(&h, with_own);
}

#[tokio::test(start_paused = true)]
async fn owner_agents_lets_in_only_the_owner_s_own_agents() {
    let h = started(Some(InboundPolicy::OwnerAgents)).await;
    let own = sibling(&h, "own");
    let other = foreign_agent(&h, "other");
    let alice = person(&h, "alice");
    let with = |user| h.messenger.conversation_with(user);
    h.messenger.says(own, "hi");
    h.messenger.says(other, "hi");
    h.messenger.says(alice, "hi");
    h.messenger
        .wait_until("the echo", |_| {
            plain_to(&h, with(own)).contains(&"echo: hi".to_owned())
        })
        .await;
    refused(&h, with(other)).await;
    refused(&h, with(alice)).await;
    settle().await;
    untouched(&h, with(other));
    untouched(&h, with(alice));
}

#[tokio::test(start_paused = true)]
async fn contacts_lets_in_every_accepted_contact() {
    let h = started(Some(InboundPolicy::Contacts)).await;
    let other = foreign_agent(&h, "other");
    let alice = person(&h, "alice");
    let stranger = h.messenger.add_user("stranger", false);
    let with = |user| h.messenger.conversation_with(user);
    h.messenger.says(other, "hi");
    h.messenger.says(alice, "hi");
    h.messenger.says(stranger, "hi");
    h.messenger
        .wait_until("the echoes", |_| {
            plain_to(&h, with(other)).contains(&"echo: hi".to_owned())
                && replies_to(&h, with(alice)).contains(&"echo: hi".to_owned())
        })
        .await;
    refused(&h, with(stranger)).await;
    settle().await;
    untouched(&h, with(stranger));
}

#[tokio::test(start_paused = true)]
async fn a_settings_change_applies_at_once() {
    let h = started(None).await;
    let alice = person(&h, "alice");
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "one");
    refused(&h, with_alice).await;
    h.messenger.change_inbound(InboundPolicy::Contacts);
    h.messenger.says(alice, "two");
    h.messenger
        .wait_until("the echo", |_| {
            replies_to(&h, with_alice).contains(&"echo: two".to_owned())
        })
        .await;
    h.messenger.change_inbound(InboundPolicy::OwnerOnly);
    h.messenger.says(alice, "three");
    settle().await;
    assert_eq!(replies_to(&h, with_alice), [NOTICE, "echo: two"]);
}

#[tokio::test(start_paused = true)]
async fn without_its_settings_the_agent_stays_owner_only() {
    let mut h = Harness::echo(300);
    h.messenger.set_inbound(InboundPolicy::Contacts);
    h.messenger
        .fail_next("agent_settings", server_error("unknown_request"));
    h.start();
    settle().await;
    let alice = person(&h, "alice");
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "hi");
    refused(&h, with_alice).await;
    settle().await;
    untouched(&h, with_alice);
}

#[tokio::test(start_paused = true)]
async fn the_settings_are_read_again_after_a_reconnect() {
    let h = started(None).await;
    let alice = person(&h, "alice");
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.set_inbound(InboundPolicy::Contacts);
    for state in [
        ConnectionState::Connected,
        ConnectionState::Connecting,
        ConnectionState::Connected,
    ] {
        h.messenger.inject(Event::ConnectionStateChanged(state));
    }
    h.messenger
        .wait_until("settings read twice", |calls| {
            calls
                .iter()
                .filter(|call| matches!(call, Call::AgentSettings))
                .count()
                == 2
        })
        .await;
    settle().await;
    h.messenger.says(alice, "hi");
    h.messenger
        .wait_until("the echo", |_| {
            replies_to(&h, with_alice).contains(&"echo: hi".to_owned())
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn agents_get_plain_text_without_the_notice_and_people_rich_replies() {
    let h = started(Some(InboundPolicy::Contacts)).await;
    let (other, alice) = (foreign_agent(&h, "other"), person(&h, "alice"));
    let (with_other, with_alice) = (
        h.messenger.conversation_with(other),
        h.messenger.conversation_with(alice),
    );
    h.messenger.says(other, "one");
    h.messenger.says(other, "two");
    h.messenger.says(alice, "hi");
    h.messenger
        .wait_until("the echoes", |_| {
            plain_to(&h, with_other).len() == 2 && replies_to(&h, with_alice).len() == 2
        })
        .await;
    assert_eq!(plain_to(&h, with_other), ["echo: one", "echo: two"]);
    assert!(
        replies_to(&h, with_other).is_empty(),
        "no notice for agents"
    );
    assert_eq!(replies_to(&h, with_alice), [NOTICE, "echo: hi"]);
    assert!(plain_to(&h, with_alice).is_empty());

    // An agent's commands are a contact's: answered in rich replies.
    h.messenger.says(other, "/help");
    h.messenger
        .wait_until("help", |_| !replies_to(&h, with_other).is_empty())
        .await;
    assert!(replies_to(&h, with_other)[0].starts_with(super::contacts::CONTACT_HELP));

    // The agent's profile was read once.
    let reads = h
        .messenger
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::Peers { users } if users.contains(&other)))
        .count();
    assert_eq!(reads, 1);
}
