//! The echo backend ([`crate::echo`]) against the core and a [`TestMessenger`], in paused
//! time: echoes, `ask` and `pick` cards with their answer rules, the timeout, paging,
//! old texts at startup, and its commands. Its voice call is in `voice_tests`.

use std::time::Duration;

use push_sdk::{ActivityState, Event, MessageKind, UserId};

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
    assert_eq!(help, commands::help(|_| false));
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
