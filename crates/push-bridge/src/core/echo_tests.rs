//! The echo backend ([`crate::echo`]) against the core and a [`TestMessenger`], in paused
//! time: echoes, `ask` and `pick` cards with their answer rules, the timeout, paging,
//! old texts at startup, and its commands. Its voice call is in `voice_tests`.

use std::time::Duration;

use push_sdk::{ActivityState, ContactState, Event, MessageKind, UserId};

use super::tests::{Harness, settle};
use crate::commands;
use crate::echo::NAME;
use crate::messenger::{Call, TestMessenger, param, template};
use crate::render::TEXT_PAGE;

/// The latest body of every agent reply (sends and their edits), in the order sent:
/// `(template, text)`.
fn replies_in(calls: &[Call]) -> Vec<(String, String)> {
    let mut bodies: Vec<(push_sdk::ClientMsgId, String)> = Vec::new();
    for call in calls {
        match call {
            Call::SendRich { id, body, .. } => bodies.push((*id, body.clone())),
            Call::EditRich { id, body, .. } => {
                if let Some(entry) = bodies.iter_mut().find(|(sent, _)| sent == id) {
                    entry.1 = body.clone();
                }
            }
            _ => {}
        }
    }
    bodies
        .into_iter()
        .map(|(_, body)| (template(&body), body))
        .filter(|(tpl, _)| tpl.starts_with("agent.reply@"))
        .map(|(tpl, body)| (tpl, param(&body, "text").unwrap_or_default()))
        .collect()
}

fn texts(messenger: &TestMessenger) -> Vec<String> {
    replies_in(&messenger.calls())
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

/// Waits for a reply that is exactly `text`.
async fn reply(h: &Harness, text: &str) {
    h.messenger
        .wait_until(text, |calls| {
            replies_in(calls).iter().any(|(_, shown)| shown == text)
        })
        .await;
}

async fn started(answer_timeout_s: u64) -> Harness {
    let mut h = Harness::echo(answer_timeout_s);
    h.start();
    settle().await;
    h
}

fn activity(h: &Harness, state: ActivityState, label: &str) -> bool {
    h.messenger
        .activities()
        .contains(&(state, Some(label.to_owned())))
}

#[tokio::test(start_paused = true)]
async fn the_owner_s_text_is_echoed_as_markdown_while_echoing() {
    let h = started(300).await;
    h.messenger.owner_says("hello **world**");
    reply(&h, "echo: hello **world**").await;
    h.idle().await;
    let replies = replies_in(&h.messenger.calls());
    assert_eq!(
        replies,
        [(
            "agent.reply@2".to_owned(),
            "echo: hello **world**".to_owned()
        )]
    );
    assert!(activity(&h, ActivityState::Working, "echoing"));
    let status = h.control_status();
    assert_eq!(status.backend.unwrap().name, NAME);
}

#[tokio::test(start_paused = true)]
async fn a_long_echo_is_split_over_several_replies() {
    let h = started(300).await;
    let text = "word ".repeat(2_000);
    h.messenger.owner_says(&text);
    h.messenger
        .wait_until("three replies", |calls| replies_in(calls).len() >= 3)
        .await;
    h.idle().await;
    let pages = texts(&h.messenger);
    assert_eq!(
        pages.len(),
        3,
        "{:?}",
        pages.iter().map(String::len).collect::<Vec<_>>()
    );
    assert!(pages[0].starts_with("echo: word word"));
    assert!(pages.iter().all(|page| page.chars().count() <= TEXT_PAGE));
    let words: usize = pages.iter().map(|page| page.matches("word").count()).sum();
    assert_eq!(words, 2_000);
}

#[tokio::test(start_paused = true)]
async fn ask_posts_a_question_only_the_asker_answers() {
    let h = started(300).await;
    h.messenger.owner_says("ask");
    let card = h.card(1).await;
    let (_, body) = h
        .messenger
        .sent_rich()
        .into_iter()
        .find(|(id, _)| *id == card)
        .unwrap();
    assert_eq!(template(&body), "agent.question@1");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let labels: Vec<_> = json["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["label"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(labels, ["Red", "Blue"]);
    h.messenger
        .wait_until("waiting", |_| {
            activity(&h, ActivityState::Waiting, "question open")
        })
        .await;

    let stranger = UserId::new();
    h.messenger.answer_as(stranger, card, &["o1"]);
    h.messenger.says(stranger, "Red");
    settle().await;
    assert!(
        !texts(&h.messenger)
            .iter()
            .any(|text| text.starts_with("you chose"))
    );
    assert!(
        h.messenger
            .texts()
            .iter()
            .any(|(_, text)| text == super::PRIVATE)
    );

    h.messenger.answer(card, &["o2"]);
    reply(&h, "you chose: Blue").await;
    h.idle().await;
}

#[tokio::test(start_paused = true)]
async fn the_asker_s_text_while_the_card_is_open_is_the_answer() {
    let h = started(300).await;
    h.messenger.owner_says("Ask");
    h.card(1).await;
    h.messenger.owner_says("purple");
    reply(&h, "you chose: purple").await;
    h.idle().await;
    assert!(
        !texts(&h.messenger)
            .iter()
            .any(|text| text.starts_with("echo:"))
    );
}

#[tokio::test(start_paused = true)]
async fn pick_posts_a_multi_select_and_takes_any_number() {
    let h = started(300).await;
    h.messenger.owner_says("pick");
    let card = h.card(1).await;
    assert_eq!(h.cards()[0].1, "agent.choose@1");
    h.messenger.answer(card, &["o3", "o1"]);
    reply(&h, "you chose: Red, Green").await;
    h.messenger.owner_says("pick");
    h.card(2).await;
    h.messenger.owner_says("blue, green, red");
    reply(&h, "you chose: Red, Blue, Green").await;
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_card_is_skipped_after_the_answer_timeout() {
    let h = started(10).await;
    h.messenger.owner_says("ask");
    h.card(1).await;
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert!(!h.replies().iter().any(|text| text == "No answer; skipped"));
    tokio::time::sleep(Duration::from_secs(2)).await;
    h.reply_containing("No answer; skipped").await;
    h.idle().await;
    h.messenger.owner_says("still there?");
    reply(&h, "echo: still there?").await;
    assert!(
        !texts(&h.messenger)
            .iter()
            .any(|text| text.starts_with("you chose"))
    );
}

#[tokio::test(start_paused = true)]
async fn texts_older_than_a_minute_at_startup_are_not_echoed() {
    let h = started(300).await;
    let conversation = h.messenger.conversation();
    let mut old = h.messenger.message(
        conversation,
        h.messenger.owner_id(),
        MessageKind::Text,
        "old",
    );
    old.server_ts = old.server_ts.map(|ts| ts - 61_000_000);
    h.messenger.inject(Event::MessageAdded(old));
    let mut recent = h.messenger.message(
        conversation,
        h.messenger.owner_id(),
        MessageKind::Text,
        "recent",
    );
    recent.server_ts = recent.server_ts.map(|ts| ts - 50_000_000);
    h.messenger.inject(Event::MessageAdded(recent));
    reply(&h, "echo: recent").await;
    h.idle().await;
    assert_eq!(texts(&h.messenger), ["echo: recent"]);
}

#[tokio::test(start_paused = true)]
async fn only_help_and_status_are_offered() {
    let h = started(300).await;
    let unsupported = [
        "/new",
        "/cd ..",
        "/sessions",
        "/resume s",
        "/model",
        "/tasks",
        "/mode plan",
        "/yolo",
        "/compact",
        "/btw hi",
    ];
    for command in unsupported {
        h.messenger.owner_says(command);
    }
    settle().await;
    let not_supported = format!("Not supported by {NAME}.");
    assert_eq!(h.replies(), vec![not_supported; unsupported.len()]);

    h.messenger.owner_says("/help");
    h.reply_containing("/status").await;
    let help = h.replies().last().unwrap().clone();
    assert_eq!(
        help,
        format!(
            "{}\n{}\n\n{}",
            commands::help(|_| false),
            crate::echo::HELP,
            commands::OWNER_NOTE
        )
    );
    assert!(!help.contains("/new") && !help.contains("/cd"));

    h.messenger.owner_says("/status");
    h.reply_containing("Echo: ").await;
    let status = h.replies().last().unwrap().clone();
    assert!(
        status.contains(&format!("Echo: {}", env!("CARGO_PKG_VERSION"))),
        "{status}"
    );
}

#[tokio::test(start_paused = true)]
async fn restarting_the_echo_agent_with_a_card_open_ends_its_turn_once() {
    let mut h = started(300).await;
    h.messenger.owner_says("ask");
    h.card(1).await;
    let restarted = h.call(crate::control::Action::RestartBackend).await;
    assert!(restarted.is_ok(), "{restarted:?}");
    h.messenger.owner_says("after");
    reply(&h, "echo: after").await;
    h.idle().await;
    let ended = h
        .control_events()
        .into_iter()
        .filter(|event| matches!(event, push_bridge_control::Event::TurnEnded { .. }))
        .count();
    assert_eq!(ended, 2, "the ask turn (cut by the core) and the echo");
}

#[tokio::test(start_paused = true)]
async fn stop_declines_an_open_question_and_echoing_goes_on() {
    let h = started(300).await;
    h.messenger.owner_says("ask");
    h.card(1).await;
    h.messenger.owner_says("/stop");
    h.reply_containing("Declined: the turn was stopped.").await;
    h.idle().await;
    h.messenger.owner_says("again");
    reply(&h, "echo: again").await;
}

/// The latest text of every agent reply sent into `conversation`, in order.
fn replies_to(calls: &[Call], conversation: push_sdk::ConversationId) -> Vec<String> {
    let mut bodies: Vec<(push_sdk::ClientMsgId, String)> = Vec::new();
    for call in calls {
        match call {
            Call::SendRich {
                conversation: to,
                id,
                body,
            } if *to == conversation => bodies.push((*id, body.clone())),
            Call::EditRich { id, body, .. } => {
                if let Some(entry) = bodies.iter_mut().find(|(sent, _)| sent == id) {
                    entry.1 = body.clone();
                }
            }
            _ => {}
        }
    }
    bodies
        .into_iter()
        .filter(|(_, body)| template(body).starts_with("agent.reply@"))
        .filter_map(|(_, body)| param(&body, "text"))
        .collect()
}

/// The cards sent into `conversation`: `(id, template)`.
fn cards_to(
    calls: &[Call],
    conversation: push_sdk::ConversationId,
) -> Vec<(push_sdk::ClientMsgId, String)> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::SendRich {
                conversation: to,
                id,
                body,
            } if *to == conversation && !template(body).starts_with("agent.reply@") => {
                Some((*id, template(body)))
            }
            _ => None,
        })
        .collect()
}

/// Waits for a reply that is exactly `text` in `conversation`.
async fn reply_in(h: &Harness, conversation: push_sdk::ConversationId, text: &str) {
    h.messenger
        .wait_until(text, |calls| {
            replies_to(calls, conversation)
                .iter()
                .any(|shown| shown == text)
        })
        .await;
}

/// The `count`th card in `conversation`, once sent.
async fn card_in(
    h: &Harness,
    conversation: push_sdk::ConversationId,
    count: usize,
) -> push_sdk::ClientMsgId {
    h.messenger
        .wait_until("a card", |calls| {
            cards_to(calls, conversation).len() >= count
        })
        .await;
    cards_to(&h.messenger.calls(), conversation)[count - 1].0
}

/// A new user who is the agent's accepted contact.
fn contact(h: &Harness) -> UserId {
    let user = UserId::new();
    h.messenger.set_contact(user, ContactState::Accepted);
    user
}

fn activities_in(
    h: &Harness,
    conversation: push_sdk::ConversationId,
) -> Vec<(ActivityState, Option<String>)> {
    h.messenger
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Activity {
                conversation: to,
                state,
                label,
            } if to == conversation => Some((state, label)),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_accepted_contact_s_text_is_echoed_in_their_conversation() {
    let h = started(300).await;
    let alice = contact(&h);
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "hi *there*");
    reply_in(&h, with_alice, "echo: hi *there*").await;
    settle().await;
    let calls = h.messenger.calls();
    assert!(replies_to(&calls, h.messenger.conversation()).is_empty());
    assert!(calls.iter().any(|call| matches!(call,
        Call::SendRich { conversation, body, .. }
            if *conversation == with_alice && template(body) == "agent.reply@2")));
    assert_eq!(
        activities_in(&h, with_alice),
        [
            (ActivityState::Working, Some("echoing".to_owned())),
            (ActivityState::Idle, None)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn strangers_requests_and_blocked_users_are_refused() {
    let h = started(300).await;
    for state in [
        ContactState::None,
        ContactState::RequestedIn,
        ContactState::RequestedOut,
        ContactState::Blocked,
    ] {
        let user = UserId::new();
        h.messenger.set_contact(user, state);
        h.messenger.says(user, "hello");
        let with_user = h.messenger.conversation_with(user);
        h.messenger
            .wait_until("private", |_| {
                h.messenger
                    .texts()
                    .contains(&(with_user, super::PRIVATE.to_owned()))
            })
            .await;
        settle().await;
        assert!(
            replies_to(&h.messenger.calls(), with_user).is_empty(),
            "{state:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_contact_who_becomes_accepted_is_echoed_after_the_change() {
    let h = started(300).await;
    let bob = UserId::new();
    let with_bob = h.messenger.conversation_with(bob);
    h.messenger.says(bob, "first");
    settle().await;
    assert!(replies_to(&h.messenger.calls(), with_bob).is_empty());
    h.messenger.inject(Event::ContactChanged(push_sdk::Contact {
        user_id: bob,
        state: ContactState::Accepted,
    }));
    h.messenger.set_contact(bob, ContactState::Accepted);
    h.messenger.says(bob, "second");
    reply_in(&h, with_bob, "echo: second").await;
}

#[tokio::test(start_paused = true)]
async fn only_the_asker_answers_their_card() {
    let h = started(300).await;
    let (alice, bob) = (contact(&h), contact(&h));
    let (with_alice, with_bob) = (
        h.messenger.conversation_with(alice),
        h.messenger.conversation_with(bob),
    );
    h.messenger.says(alice, "ask");
    let alice_card = card_in(&h, with_alice, 1).await;
    assert_eq!(
        cards_to(&h.messenger.calls(), with_alice)[0].1,
        "agent.question@1"
    );
    h.messenger
        .wait_until("waiting", |_| {
            activities_in(&h, with_alice)
                .contains(&(ActivityState::Waiting, Some("question open".to_owned())))
        })
        .await;
    h.messenger.owner_says("ask");
    let owner_card = card_in(&h, h.messenger.conversation(), 1).await;

    // Anyone else's answer to a card is ignored; the cards stay open.
    h.messenger.answer_in(with_alice, bob, alice_card, &["o1"]);
    h.messenger
        .answer_in(with_alice, h.messenger.owner_id(), alice_card, &["o1"]);
    h.messenger.answer_as(alice, owner_card, &["o1"]);
    // Bob's own text is just echoed, in his conversation.
    h.messenger.says(bob, "Red");
    reply_in(&h, with_bob, "echo: Red").await;
    settle().await;
    let chose = |conversation| {
        replies_to(&h.messenger.calls(), conversation)
            .iter()
            .any(|text| text.starts_with("you chose"))
    };
    assert!(!chose(with_alice) && !chose(h.messenger.conversation()));

    h.messenger
        .answer_in(with_alice, alice, alice_card, &["o2"]);
    reply_in(&h, with_alice, "you chose: Blue").await;
    h.messenger.answer(owner_card, &["o1"]);
    reply(&h, "you chose: Red").await;
    assert!(!chose(with_bob));
}

#[tokio::test(start_paused = true)]
async fn a_contact_answers_by_text_and_picks_several() {
    let h = started(300).await;
    let alice = contact(&h);
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "pick");
    card_in(&h, with_alice, 1).await;
    assert_eq!(
        cards_to(&h.messenger.calls(), with_alice)[0].1,
        "agent.choose@1"
    );
    h.messenger.says(alice, "green, red");
    reply_in(&h, with_alice, "you chose: Red, Green").await;
    h.messenger.says(alice, "ask");
    card_in(&h, with_alice, 2).await;
    h.messenger.says(alice, "teal");
    reply_in(&h, with_alice, "you chose: teal").await;
    h.messenger.says(alice, "ask");
    let card = card_in(&h, with_alice, 3).await;
    h.messenger.answer_in(with_alice, alice, card, &["o1"]);
    reply_in(&h, with_alice, "you chose: Red").await;
}

#[tokio::test(start_paused = true)]
async fn each_contact_s_card_times_out_on_its_own() {
    let h = started(10).await;
    let (alice, bob) = (contact(&h), contact(&h));
    let (with_alice, with_bob) = (
        h.messenger.conversation_with(alice),
        h.messenger.conversation_with(bob),
    );
    h.messenger.says(alice, "ask");
    card_in(&h, with_alice, 1).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    h.messenger.says(bob, "ask");
    card_in(&h, with_bob, 1).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    reply_in(&h, with_alice, "No answer; skipped").await;
    assert!(
        !replies_to(&h.messenger.calls(), with_bob)
            .iter()
            .any(|text| text == "No answer; skipped")
    );
    h.messenger.says(alice, "after");
    reply_in(&h, with_alice, "echo: after").await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    reply_in(&h, with_bob, "No answer; skipped").await;
}

/// The notice the test messenger's agent and owner give.
const NOTICE: &str =
    "Chats with Coda on Devbox are visible to its owner, Test Owner (@test-owner).";

#[tokio::test(start_paused = true)]
async fn every_contact_is_told_once_that_the_owner_can_read_the_chat() {
    let h = started(300).await;
    let (alice, bob) = (contact(&h), contact(&h));
    let (with_alice, with_bob) = (
        h.messenger.conversation_with(alice),
        h.messenger.conversation_with(bob),
    );
    h.messenger.says(alice, "one");
    reply_in(&h, with_alice, "echo: one").await;
    h.messenger.says(alice, "two");
    reply_in(&h, with_alice, "echo: two").await;
    h.messenger.says(bob, "hi");
    reply_in(&h, with_bob, "echo: hi").await;
    let calls = h.messenger.calls();
    assert_eq!(
        replies_to(&calls, with_alice),
        [NOTICE, "echo: one", "echo: two"]
    );
    assert_eq!(replies_to(&calls, with_bob), [NOTICE, "echo: hi"]);
    assert_eq!(
        super::contacts::notice(None, None),
        "Chats with this agent are visible to its owner."
    );
}

#[tokio::test(start_paused = true)]
async fn another_agents_replies_and_refusals_never_start_a_loop() {
    let h = started(300).await;
    let other_agent = contact(&h);
    let with_other = h.messenger.conversation_with(other_agent);
    // An accepted contact's card or reply (another bridge's echo) is not a prompt.
    let reply = r#"{"tpl":"agent.reply@1","params":{"text":"echo: hi"},"fallback":"echo: hi"}"#;
    h.messenger.inject(Event::MessageAdded(h.messenger.message(
        with_other,
        other_agent,
        MessageKind::Rich,
        reply,
    )));
    // A stranger bridge's "This agent is private." is not answered with another.
    let stranger = UserId::new();
    h.messenger.says(stranger, super::PRIVATE);
    settle().await;
    assert!(replies_to(&h.messenger.calls(), with_other).is_empty());
    assert!(h.messenger.texts().is_empty(), "{:?}", h.messenger.texts());
}

#[tokio::test(start_paused = true)]
async fn contacts_may_run_only_help_and_status() {
    let h = started(300).await;
    let alice = contact(&h);
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "/help");
    reply_in(
        &h,
        with_alice,
        &format!("{}\n\n{NOTICE}", super::contacts::CONTACT_HELP),
    )
    .await;
    h.messenger.says(alice, "/status");
    reply_in(
        &h,
        with_alice,
        &format!("{NAME}: {}", env!("CARGO_PKG_VERSION")),
    )
    .await;
    for command in ["/new", "/stop", "/cd ..", "/model", "/mode bypass"] {
        h.messenger.says(alice, command);
    }
    h.messenger
        .wait_until("refusals", |calls| {
            replies_to(calls, with_alice)
                .iter()
                .filter(|text| *text == super::contacts::OWNER_ONLY)
                .count()
                == 5
        })
        .await;
    h.messenger.says(alice, "/shrug");
    reply_in(&h, with_alice, "echo: /shrug").await;
    assert!(
        h.replies()
            .iter()
            .all(|text| !text.contains("Not supported"))
    );
    assert!(replies_to(&h.messenger.calls(), h.messenger.conversation()).is_empty());
}

#[tokio::test(start_paused = true)]
async fn coda_still_answers_only_its_owner() {
    let mut h = Harness::named("Coda");
    h.start();
    h.backend.wait_for("start", 1).await;
    settle().await;
    let alice = contact(&h);
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "hi");
    h.messenger
        .wait_until("private", |_| {
            h.messenger
                .texts()
                .contains(&(with_alice, super::PRIVATE.to_owned()))
        })
        .await;
    settle().await;
    assert_eq!(h.count("prompt"), 0);
    assert!(replies_to(&h.messenger.calls(), with_alice).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_contact_s_open_question_keeps_its_activity_fresh() {
    let h = started(300).await;
    let alice = contact(&h);
    let with_alice = h.messenger.conversation_with(alice);
    h.messenger.says(alice, "ask");
    card_in(&h, with_alice, 1).await;
    tokio::time::sleep(Duration::from_secs(31)).await;
    let waiting = (ActivityState::Waiting, Some("question open".to_owned()));
    let sent = activities_in(&h, with_alice)
        .into_iter()
        .filter(|activity| *activity == waiting)
        .count();
    assert!(sent >= 3, "sent {sent} times in 31 s");
    h.messenger.says(alice, "red");
    reply_in(&h, with_alice, "you chose: Red").await;
    settle().await;
    assert_eq!(
        activities_in(&h, with_alice).last(),
        Some(&(ActivityState::Idle, None))
    );
    let before = activities_in(&h, with_alice).len();
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(
        activities_in(&h, with_alice).len(),
        before,
        "idle is sent once"
    );
}

/// A server refusal naming the waiting action `id`.
fn waiting_for_owner(id: &str) -> push_sdk::SdkError {
    push_sdk::SdkError::Server {
        code: "waiting_for_owner".into(),
        message: "waits".into(),
        retryable: false,
        retry_after_s: None,
        action_id: Some(id.into()),
    }
}

#[tokio::test(start_paused = true)]
async fn the_owner_s_commands_run_bridge_actions_and_echo_their_result() {
    let h = started(300).await;
    let bob = h.messenger.add_user("bob", false);
    h.messenger.owner_says("send @bob hi there");
    reply(&h, "done: sent to @bob").await;
    reply_in(&h, h.messenger.conversation_with(bob), "hi there").await;
    h.idle().await;

    h.messenger.owner_says("SEND owner hello me");
    reply(&h, "done: sent to the owner").await;
    assert!(texts(&h.messenger).contains(&"hello me".to_owned()));
    h.idle().await;

    h.messenger.owner_says("notify all green");
    reply(&h, "done: the owner was notified").await;
    h.idle().await;
    assert!(h.messenger.texts().is_empty(), "people get rich replies");
}

#[tokio::test(start_paused = true)]
async fn refusals_and_malformed_commands_are_echoed_as_one_line() {
    let h = started(300).await;
    h.messenger.add_user("bob", false);
    let cases = [
        (
            "send @bob hi",
            Some(("open_direct", waiting_for_owner("a-1"))),
            "waiting for owner (action a-1)",
        ),
        (
            "request @bob",
            Some((
                "request_contact",
                crate::messenger::server_error("not_allowed_by_owner"),
            )),
            "not allowed by owner",
        ),
        (
            "request @bob",
            Some((
                "request_contact",
                push_sdk::SdkError::Server {
                    code: "rate_limited".into(),
                    message: "slow down".into(),
                    retryable: true,
                    retry_after_s: Some(30),
                    action_id: None,
                },
            )),
            "rate limited, retry in 30 s",
        ),
        ("request @nobody", None, "not found: no user @nobody"),
        (
            "send bob hi",
            None,
            "usage: send @handle|owner|group:<id> <text>",
        ),
        (
            "call owner",
            None,
            "unavailable: voice calls are not set up",
        ),
    ];
    for (command, failure, answer) in cases {
        if let Some((method, error)) = failure {
            h.messenger.fail_next(method, error);
        }
        h.messenger.owner_says(command);
        reply(&h, answer).await;
        h.idle().await;
    }
    let kept = h.state.get().pending_actions;
    assert_eq!(kept.keys().collect::<Vec<_>>(), ["a-1"]);
}

#[tokio::test(start_paused = true)]
async fn a_contact_s_command_words_are_only_echoed() {
    let h = started(300).await;
    let alice = contact(&h);
    h.messenger.add_user("bob", false);
    h.messenger.says(alice, "send @bob hi");
    reply_in(
        &h,
        h.messenger.conversation_with(alice),
        "echo: send @bob hi",
    )
    .await;
    settle().await;
    assert!(
        !h.messenger
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Lookup { .. }))
    );
}
