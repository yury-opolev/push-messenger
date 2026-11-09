//! The inbound policy, agent turns and the loop guard (spec 2026-11-09 proactive agents
//! §4, §7.2, §7.4) with the echo backend: who besides the owner reaches it, how it answers
//! agents and people, and how a chat between agents is paused.

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
async fn the_backend_learns_the_contact_s_handle_and_kind() {
    let mut h = Harness::named("Test");
    h.backend.set_accepts_contacts(true);
    h.messenger.set_inbound(InboundPolicy::Contacts);
    h.start();
    settle().await;
    let other = foreign_agent(&h, "other");
    let alice = person(&h, "alice");
    h.messenger.says(other, "hi");
    h.backend.wait_for("contact", 1).await;
    h.messenger.says(alice, "hello");
    h.backend.wait_for("contact", 2).await;
    assert_eq!(
        h.backend.contact_infos(),
        [
            crate::backend::ContactInfo {
                handle: Some("other".into()),
                agent: true
            },
            crate::backend::ContactInfo {
                handle: Some("alice".into()),
                agent: false
            }
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_contact_s_group_text_opens_no_chat_and_reaches_nobody() {
    let h = started(Some(InboundPolicy::Contacts)).await;
    h.messenger.gate_chats(server_error("waiting_for_owner"));
    let (alice, carol) = (person(&h, "alice"), person(&h, "carol"));
    let group = h.messenger.add_group();
    for sender in [alice, carol] {
        let text = h
            .messenger
            .message(group, sender, push_sdk::MessageKind::Text, "hi all");
        h.messenger.inject(Event::MessageAdded(text));
    }
    settle().await;
    assert!(replies_to(&h, group).is_empty());
    assert!(plain_to(&h, group).is_empty());
    assert!(!h.messenger.has_direct(carol));
    // Alice's own chat still reaches the backend.
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "hi");
    h.messenger
        .wait_until("the echo", |_| {
            replies_to(&h, with_alice).contains(&"echo: hi".to_owned())
        })
        .await;
    assert!(h.messenger.opened_directs().is_empty(), "no chat opened");
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

// The loop guard (spec §7.4).

/// The echo agent open to contacts, pausing a chat with an agent after `turns`.
async fn guarded(turns: u32) -> Harness {
    let mut h = Harness::echo(300);
    h.loop_guard_turns = turns;
    h.messenger.set_inbound(InboundPolicy::Contacts);
    h.start();
    settle().await;
    h
}

/// The rich replies' latest texts in the owner's chat (sends and their edits).
fn owner_texts(h: &Harness) -> Vec<String> {
    let owner = h.messenger.conversation();
    let mut shown: Vec<(push_sdk::ClientMsgId, String)> = Vec::new();
    for call in h.messenger.calls() {
        match call {
            Call::SendRich {
                conversation,
                id,
                body,
            } if conversation == owner && template(&body).starts_with("agent.reply@") => {
                shown.push((id, param(&body, "text").unwrap_or_default()));
            }
            Call::EditRich { id, body, .. } => {
                if let Some(entry) = shown.iter_mut().find(|(sent, _)| *sent == id) {
                    entry.1 = param(&body, "text").unwrap_or_default();
                }
            }
            _ => {}
        }
    }
    shown.into_iter().map(|(_, text)| text).collect()
}

/// Waits for an owner's-chat reply containing `text`.
async fn owner_saw(h: &Harness, text: &str) {
    h.messenger
        .wait_until(text, |_| {
            owner_texts(h).iter().any(|shown| shown.contains(text))
        })
        .await;
}

/// The echo agent's echoes (plain texts other than the private notice) in `conversation`.
fn echoes(h: &Harness, conversation: ConversationId) -> Vec<String> {
    plain_to(h, conversation)
        .into_iter()
        .filter(|text| text != super::PRIVATE)
        .collect()
}

/// `peer` writes `text`, and the echo agent's answer (its `count`th echo) is awaited.
async fn turn(h: &Harness, peer: push_sdk::UserId, text: &str, count: usize) {
    let conversation = h.messenger.conversation_with(peer);
    h.messenger.says(peer, text);
    h.messenger
        .wait_until("the echo", |_| echoes(h, conversation).len() >= count)
        .await;
}

const NOTE_4: &str = "Paused after 4 turns between agents without a person. An owner can resume.";
const OWNER_NOTE_4: &str = "Paused the chat with @peer after 4 agent turns without a person. \
Send /loop resume to let it continue.";

#[tokio::test(start_paused = true)]
async fn two_agents_stop_at_the_limit_and_say_so_once() {
    let h = guarded(4).await;
    let peer = foreign_agent(&h, "peer");
    let with_peer = h.messenger.conversation_with(peer);
    // The other agent answers each echo, as another echo agent would: turns 1 to 4.
    turn(&h, peer, "hi", 1).await;
    turn(&h, peer, "echo: hi", 2).await;
    owner_saw(&h, OWNER_NOTE_4).await;
    assert_eq!(replies_to(&h, with_peer), [NOTE_4]);
    // Turn 5 and on are not passed on.
    h.messenger.says(peer, "echo: echo: echo: hi");
    h.messenger.says(peer, "anyone?");
    settle().await;
    assert_eq!(echoes(&h, with_peer), ["echo: hi", "echo: echo: hi"]);
    assert_eq!(replies_to(&h, with_peer), [NOTE_4]);
    let owner_notes = owner_texts(&h)
        .iter()
        .filter(|text| *text == OWNER_NOTE_4)
        .count();
    assert_eq!(owner_notes, 1);
}

#[tokio::test(start_paused = true)]
async fn send_message_counts_and_is_refused_in_a_paused_chat() {
    let h = guarded(4).await;
    let peer = foreign_agent(&h, "peer");
    let with_peer = h.messenger.conversation_with(peer);
    h.messenger.owner_says("send @peer one");
    owner_saw(&h, "done: sent to @peer").await;
    turn(&h, peer, "two", 2).await;
    h.messenger.owner_says("send @peer four");
    owner_saw(&h, OWNER_NOTE_4).await;
    h.messenger.owner_says("send @peer five");
    owner_saw(
        &h,
        "paused: the loop guard paused this chat; the owner can resume it with /loop resume",
    )
    .await;
    assert_eq!(echoes(&h, with_peer), ["one", "echo: two", "four"]);
}

#[tokio::test(start_paused = true)]
async fn a_late_reply_to_a_paused_agent_is_dropped() {
    let h = guarded(2).await;
    let peer = foreign_agent(&h, "peer");
    let with_peer = h.messenger.conversation_with(peer);
    h.messenger.says(peer, "ask");
    h.messenger
        .wait_until("the card", |calls| {
            calls.iter().any(|call| {
                matches!(call,
                Call::SendRich { conversation, body, .. }
                    if *conversation == with_peer && template(body) == "agent.question@1")
            })
        })
        .await;
    let card = h
        .messenger
        .sent_rich()
        .into_iter()
        .find(|(_, body)| template(body) == "agent.question@1")
        .unwrap()
        .0;
    h.messenger.says(peer, "hello");
    h.messenger
        .wait_until("paused", |_| {
            replies_to(&h, with_peer).contains(
                &"Paused after 2 turns between agents without a person. An owner can resume."
                    .to_owned(),
            )
        })
        .await;
    h.messenger.answer_in(with_peer, peer, card, &["o1"]);
    settle().await;
    assert!(
        echoes(&h, with_peer).is_empty(),
        "{:?}",
        echoes(&h, with_peer)
    );
}

#[tokio::test(start_paused = true)]
async fn the_owner_lists_and_resumes_paused_chats() {
    let h = guarded(4).await;
    let peer = foreign_agent(&h, "peer");
    let with_peer = h.messenger.conversation_with(peer);
    h.messenger.owner_says("/loop");
    owner_saw(&h, "No chat is paused.").await;
    turn(&h, peer, "hi", 1).await;
    turn(&h, peer, "echo: hi", 2).await;
    owner_saw(&h, OWNER_NOTE_4).await;

    h.messenger.owner_says("/loop");
    owner_saw(&h, "Paused chats with other agents:\n@peer").await;
    h.messenger.owner_says("/loop resume @nobody");
    owner_saw(&h, "No paused chat with @nobody.").await;
    h.messenger.owner_says("/loop resume @peer");
    owner_saw(&h, "Resumed the chat with @peer.").await;
    turn(&h, peer, "again", 3).await;
    assert_eq!(echoes(&h, with_peer)[2], "echo: again");

    // Paused again after four more turns; `/loop resume` resumes every chat.
    turn(&h, peer, "and again", 4).await;
    h.messenger.says(peer, "and again and again");
    h.messenger
        .wait_until("paused again", |_| replies_to(&h, with_peer).len() == 2)
        .await;
    h.messenger.owner_says("/loop resume");
    h.messenger
        .wait_until("resumed again", |_| {
            owner_texts(&h)
                .iter()
                .filter(|text| *text == "Resumed the chat with @peer.")
                .count()
                == 2
        })
        .await;
    turn(&h, peer, "last", 5).await;

    // A contact may not.
    let alice = person(&h, "alice");
    h.messenger.says(alice, "/loop resume");
    h.messenger
        .wait_until("refused", |_| {
            replies_to(&h, h.messenger.conversation_with(alice))
                .contains(&super::contacts::OWNER_ONLY.to_owned())
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_person_s_message_in_the_chat_resets_the_count() {
    let h = guarded(4).await;
    let peer = foreign_agent(&h, "peer");
    let carol = person(&h, "carol");
    let with_peer = h.messenger.conversation_with(peer);
    turn(&h, peer, "one", 1).await;
    h.messenger.inject(Event::MessageAdded(h.messenger.message(
        with_peer,
        carol,
        push_sdk::MessageKind::Text,
        "carry on",
    )));
    // Without Carol's message, this echo would be the fourth turn.
    turn(&h, peer, "two", 2).await;
    settle().await;
    assert!(replies_to(&h, with_peer).is_empty(), "not paused yet");
    turn(&h, peer, "three", 3).await;
    h.messenger
        .wait_until("paused", |_| replies_to(&h, with_peer) == [NOTE_4])
        .await;
}
