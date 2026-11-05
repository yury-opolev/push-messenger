//! Voice calls in the core, against a [`FakePhone`] and the testkit's scripted voice
//! adapter (in-process, [`ScriptedStarter`]) hearing the recorded fixtures
//! ([`push_voice_adapter_testkit::fixture`]), in paused time: the fake speaker plays one
//! frame per 20 ms of tokio's clock, the adapter hears and speaks in real time.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::NaiveDateTime;
use push_bridge_control::{CallDirection, CallPhase, Event as ControlEvent, NO_CALL};
use push_sdk::{ActivityState, CallEndReason, CallEvent, CallId, Event, UserId};
use push_voice_adapter_proto::BridgeHello;
use push_voice_adapter_testkit::{
    Event as AdapterEvent, Fixture, Log, Script, Word, fixture as fixtures, tones,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::CallFlag;
use super::tests::{Harness, peeked, permission, settle};
use super::voice::{ADAPTER_BACK, ADAPTER_LOST};
use crate::backend::{Answer, BackendEvent, RequestKind, TurnSource};
use crate::config::AnswerMode;
use crate::control::Action;
use crate::messenger::{param, template};
use crate::voice::VoiceSetup;
use crate::voice::adapter::advertise::advertise;
use crate::voice::adapter::router::AdapterSetup;
use crate::voice::adapter::supervisor::Supervisor;
use crate::voice::adapter::supervisor::testing::{Plan, ScriptedStarter};
use crate::voice::confirm::APPROVE_OR_REJECT;
use crate::voice::phone::{FakeCall, FakePhone};
use crate::voice::policy::{CallPolicy, NotifyPolicy, QuietHours};
use crate::voice::speech::CODE_NOTE;

const GREETING: &str = "Connected to Coda in work.";
/// A word no fixture uses, with a secret in it.
const SECRET_HZ: f64 = 2200.0;
const SECRET_WORDS: &str = "my token=abc123def";

fn fixture(name: &str) -> Fixture {
    Fixture::named(name)
}

/// The adapter of [`Voice`]: it hears every fixture's words and [`SECRET_WORDS`], and
/// speaks 20 ms a character.
fn voice_script() -> Script {
    let mut words = fixtures::all_words();
    words.push(Word::new(SECRET_HZ, SECRET_WORDS));
    Script {
        ms_per_char: 20,
        ..Script::with_words(words)
    }
}

/// The owner says one word at `hz`.
fn utterance(hz: f64) -> Vec<i16> {
    let mut audio = tones::silence(10);
    audio.extend(tones::tone(hz, 45));
    audio.extend(tones::silence(50));
    audio
}

/// A bridge with voice, its phone and the adapters it started.
struct Voice {
    h: Harness,
    phone: Arc<FakePhone>,
    starter: Option<Arc<ScriptedStarter>>,
}

impl Voice {
    /// With a ready adapter hearing the fixtures ([`voice_script`]).
    async fn new() -> Self {
        Self::with_policy(CallPolicy::default()).await
    }

    /// With the answering and calling `policy`.
    async fn with_policy(policy: CallPolicy) -> Self {
        Self::with_script(voice_script(), policy).await
    }

    /// With an adapter playing `script`, ready, its calls advertised.
    async fn with_script(script: Script, policy: CallPolicy) -> Self {
        let a = Adapted::with_policy(then_not_found(vec![Plan::Adapter(script)]), policy).await;
        a.ready().await;
        a.setup.set_advertised(true);
        a.v
    }

    /// `caller` calls the agent.
    fn ring(&self, caller: UserId) -> CallId {
        let call_id = CallId::new();
        self.h.messenger.inject(Event::Call(CallEvent::Incoming {
            call_id,
            caller_id: caller,
            offer_sdp: "offer".into(),
            expires_at_ms: i64::MAX,
        }));
        call_id
    }

    /// The owner calls; returns once the greeting was played.
    async fn call(&self) -> FakeCall {
        let greetings = self.said(GREETING);
        self.ring(self.h.messenger.owner_id());
        let call = self.phone.wait_answered().await;
        self.until("the greeting", || self.said(GREETING) > greetings)
            .await;
        quiet().await;
        call
    }

    /// What the adapters started to speak, in order.
    fn spoken(&self) -> Vec<String> {
        self.starter.as_ref().map_or_else(Vec::new, |starter| {
            starter.logs().iter().flat_map(Log::spoken).collect()
        })
    }

    /// How often `text` was spoken.
    fn said(&self, text: &str) -> usize {
        self.spoken().iter().filter(|said| *said == text).count()
    }

    /// Waits until `text` was spoken.
    async fn until_said(&self, text: &str) {
        self.until(text, || self.said(text) > 0).await;
    }

    /// Waits until `done` holds (in tokio's clock); panics after 60 s.
    async fn until(&self, what: &str, done: impl Fn() -> bool) {
        for _ in 0..6000 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}; spoken {:?}", self.spoken());
    }

    /// The transcripts posted: `(speaker, text)`.
    fn transcripts(&self) -> Vec<(String, String)> {
        self.h
            .messenger
            .sent_rich()
            .iter()
            .filter(|(_, body)| template(body) == "agent.transcript@1")
            .map(|(_, body)| {
                (
                    param(body, "speaker").unwrap(),
                    param(body, "text").unwrap(),
                )
            })
            .collect()
    }

    /// The first card of template `tpl`, once sent.
    async fn card(&self, tpl: &str) -> push_sdk::ClientMsgId {
        let find = || {
            self.h
                .messenger
                .sent_rich()
                .into_iter()
                .find(|(_, body)| template(body) == tpl)
                .map(|(id, _)| id)
        };
        self.until(tpl, || find().is_some()).await;
        find().unwrap()
    }

    /// The edits of `card`: `(template, title, text)`.
    fn edits_of(&self, card: push_sdk::ClientMsgId) -> Vec<(String, String, String)> {
        self.h
            .messenger
            .edits()
            .into_iter()
            .filter(|(id, _)| *id == card)
            .map(|(_, body)| {
                (
                    template(&body),
                    param(&body, "title").unwrap_or_default(),
                    param(&body, "text").unwrap_or_default(),
                )
            })
            .collect()
    }

    /// The working labels sent, in order, repeats folded.
    fn labels(&self) -> Vec<String> {
        self.h
            .messenger
            .activities()
            .into_iter()
            .filter(|(state, _)| *state == ActivityState::Working)
            .filter_map(|(_, label)| label)
            .collect()
    }

    /// Waits until the activity is working with `label`.
    async fn activity(&self, label: &str) {
        let wanted = (ActivityState::Working, Some(label.to_owned()));
        self.h
            .messenger
            .wait_until(label, |_| {
                self.h.messenger.activities().last() == Some(&wanted)
            })
            .await;
    }

    /// Everything the agent posted as reply text, sends and edits.
    fn posted(&self) -> Vec<String> {
        let messenger = &self.h.messenger;
        messenger
            .sent_rich()
            .into_iter()
            .chain(messenger.edits())
            .filter(|(_, body)| template(body).starts_with("agent.reply@"))
            .filter_map(|(_, body)| param(&body, "text"))
            .collect()
    }
}

/// Lets what the agent says play out (the adapter speaks 20 ms a character).
async fn quiet() {
    tokio::time::sleep(Duration::from_secs(3)).await;
}

/// The owner says `audio` in real time (the bridge keeps 1 s of the owner's audio queued
/// for the adapter).
async fn say(call: &FakeCall, audio: &[i16]) {
    call.say_paced(audio).await;
}

/// The owner starts saying `audio` in real time.
fn saying(call: &FakeCall, audio: Vec<i16>) -> JoinHandle<()> {
    let call = call.clone();
    tokio::spawn(async move { call.say_paced(&audio).await })
}

/// Whether `wanted` appears in `seen` in order (not necessarily next to each other).
fn in_order(seen: &[String], wanted: &[&str]) -> bool {
    let mut rest = seen.iter();
    wanted
        .iter()
        .all(|wanted| rest.any(|label| label == wanted))
}

#[tokio::test(start_paused = true)]
async fn the_owner_s_call_is_answered_with_a_greeting() {
    let v = Voice::new().await;
    let call = v.call().await;
    assert_eq!(v.phone.answered(), [call.call_id]);
    assert!(v.phone.declined().is_empty());
    assert_eq!(v.spoken(), [GREETING]);
    assert!(!call.heard().is_empty(), "the greeting played");
    v.activity("on a call").await;
    assert!(in_order(
        &v.labels(),
        &["on a call", "speaking", "on a call"]
    ));
}

#[tokio::test(start_paused = true)]
async fn strangers_second_calls_and_calls_without_stt_are_declined() {
    let v = Voice::new().await;
    let owner = v.h.messenger.owner_id();
    let stranger = v.ring(UserId::new());
    v.phone.wait_declined(1).await;
    assert_eq!(v.phone.declined(), [(stranger, false)]);
    assert!(v.phone.answered().is_empty());
    assert!(v.h.messenger.texts().is_empty(), "no reply to a stranger");

    let call = v.call().await;
    let second = v.ring(owner);
    v.phone.wait_declined(2).await;
    assert_eq!(v.phone.declined()[1], (second, true));
    assert_eq!(v.phone.answered(), [call.call_id]);

    // An adapter that cannot listen takes no calls.
    let mut deaf = voice_script();
    deaf.capabilities.stt = false;
    let off = Voice::with_script(deaf, CallPolicy::default()).await;
    let first = off.ring(off.h.messenger.owner_id());
    off.phone.wait_declined(1).await;
    let again = off.ring(off.h.messenger.owner_id());
    off.phone.wait_declined(2).await;
    assert_eq!(off.phone.declined(), [(first, false), (again, false)]);
    assert!(off.phone.answered().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_utterance_is_posted_and_prompted_or_steered_into_a_running_turn() {
    let v = Voice::new().await;
    let call = v.call().await;
    let prompt = fixture("prompt");
    say(&call, &prompt.audio()).await;
    v.h.backend.wait_for("prompt run the tests", 1).await;
    assert_eq!(
        v.transcripts(),
        [("owner".to_owned(), "run the tests".to_owned())]
    );
    let peek = peeked(&v.h);
    assert_eq!(peek.last().unwrap().text, "🎤 run the tests");

    v.h.backend.turn_started(TurnSource::Prompt);
    settle().await;
    say(&call, &prompt.audio()).await;
    v.h.backend.wait_for("steer run the tests", 1).await;
    assert_eq!(v.h.count("prompt"), 1);
    assert_eq!(v.transcripts().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_reply_is_spoken_sentence_by_sentence_and_still_posted() {
    let v = Voice::new().await;
    let _call = v.call().await;
    v.h.backend.turn_started(TurnSource::Prompt);
    v.h.backend
        .text("Done. All tests pass.\n```\ncargo test\n```\nNext I'll commit.");
    v.h.backend.turn_ended(false);
    v.until_said("Next I'll commit.").await;
    assert_eq!(
        v.spoken()[1..],
        ["Done.", "All tests pass.", CODE_NOTE, "Next I'll commit."]
    );
    v.activity("on a call").await;
    assert!(
        v.posted()
            .iter()
            .any(|text| text.contains("All tests pass.") && text.contains("cargo test")),
        "{:?}",
        v.posted()
    );
}

#[tokio::test(start_paused = true)]
async fn the_owner_barges_in_and_the_steer_s_answer_is_spoken() {
    // The barge-in comes with the owner's first word, the transcript once it ended: the
    // reply goes on in between.
    let v = Voice::new().await;
    let call = v.call().await;
    let greeting = call.heard().len();
    v.h.backend.turn_started(TurnSource::Prompt);
    let sentences: Vec<String> = (1..=20)
        .map(|i| format!("Sentence {i} of the long reply has some words."))
        .collect();
    for sentence in &sentences {
        v.h.backend.text(&format!("{sentence}\n"));
    }
    call.wait_until("the reply plays", |call| {
        call.heard().len() > greeting + 16 * 400
    })
    .await;
    let owner = saying(&call, fixture("barge_in").audio());
    call.wait_until("the barge-in", |call| call.clears() >= 1)
        .await;
    settle().await;
    v.h.backend.text("This part is cut.\n");
    v.h.backend
        .wait_for("steer stop, use the other file", 1)
        .await;
    v.h.backend.text("Then I switch files.\n");
    v.h.backend.turn_ended(false);
    v.until_said("Then I switch files.").await;

    let spoken = v.spoken();
    assert!(spoken.contains(&sentences[0]), "{spoken:?}");
    assert!(!spoken.contains(&sentences[19]), "{spoken:?}");
    assert!(!spoken.iter().any(|said| said == "This part is cut."));
    assert_eq!(
        v.transcripts().last().map(|(_, text)| text.as_str()),
        Some("stop, use the other file")
    );
    assert_eq!(v.h.count("prompt"), 0);
    call.wait_until("the answer plays", |call| {
        !call.heard_since_clear().is_empty()
    })
    .await;
    owner.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reply_text_handled_before_the_barge_in_event_is_not_spoken() {
    let v = Voice::new().await;
    let call = v.call().await;
    let greeting = call.heard().len();
    v.h.backend.turn_started(TurnSource::Prompt);
    for i in 1..=20 {
        v.h.backend
            .text(&format!("Sentence {i} of the long reply has some words.\n"));
    }
    call.wait_until("the reply plays", |call| {
        call.heard().len() > greeting + 16 * 400
    })
    .await;
    // The barge-in's clear queues this delta right after the session's `BargedIn`: the
    // core, preferring backend events, handles the delta first.
    let backend = v.h.backend.clone();
    call.on_next_clear(move || backend.text("This part is cut.\n"));
    let _owner = saying(&call, fixture("barge_in").audio());
    v.h.backend
        .wait_for("steer stop, use the other file", 1)
        .await;
    v.h.backend.text("Then I switch files.\n");
    v.h.backend.turn_ended(false);
    v.until_said("Then I switch files.").await;
    let spoken = v.spoken();
    assert!(
        !spoken.iter().any(|said| said == "This part is cut."),
        "{spoken:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn approve_by_voice_answers_the_card_and_closes_it() {
    let v = Voice::new().await;
    let call = v.call().await;
    let answer = v.h.backend.request(permission());
    let card = v.card("agent.approval@1").await;
    v.until_said("Coda wants to use Bash. Approve or reject?")
        .await;
    v.h.messenger
        .wait_until("waiting", |_| {
            v.h.messenger.activities().last().map(|(state, _)| *state)
                == Some(ActivityState::Waiting)
        })
        .await;
    quiet().await;

    say(&call, &fixture("approve").audio()).await;
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
    v.until_said("Approved.").await;
    assert_eq!(
        v.edits_of(card),
        [(
            "agent.reply@1".to_owned(),
            "Allow Bash?".to_owned(),
            "🎤 Answered by voice: Approve.".to_owned()
        )]
    );
    v.h.messenger
        .wait_until("not waiting", |_| {
            v.h.messenger.activities().last().map(|(state, _)| *state)
                == Some(ActivityState::Working)
        })
        .await;

    let calls = v.h.messenger.calls().len();
    v.h.messenger.answer(card, &["reject"]);
    settle().await;
    assert_eq!(
        v.h.messenger.calls().len(),
        calls,
        "a later answer is ignored"
    );
    assert_eq!(v.h.count("prompt"), 0);
}

#[tokio::test(start_paused = true)]
async fn option_two_answers_a_question_by_voice() {
    let v = Voice::new().await;
    let call = v.call().await;
    let answer = v.h.backend.request(RequestKind::Question {
        question: "Which region?".into(),
        options: vec!["EU".into(), "US".into(), "Asia".into()],
        multi_select: false,
        allow_free_text: false,
    });
    let card = v.card("agent.question@1").await;
    quiet().await;
    say(&call, &fixture("option_two").audio()).await;
    assert_eq!(answer.await, Ok(Answer::Question("US".into())));
    v.until_said("Got it: US.").await;
    assert_eq!(
        v.edits_of(card)
            .into_iter()
            .map(|(_, _, text)| text)
            .collect::<Vec<_>>(),
        ["🎤 Answered by voice: US."]
    );
}

#[tokio::test(start_paused = true)]
async fn an_unclear_answer_is_asked_again_and_the_card_stays_open() {
    let v = Voice::new().await;
    let call = v.call().await;
    let mut answer = v.h.backend.request(permission());
    let card = v.card("agent.approval@1").await;
    quiet().await;
    let audio = fixture("ambiguous").audio();
    let (maybe, reject) = audio.split_at(16 * 2100);
    say(&call, maybe).await;
    v.until_said(APPROVE_OR_REJECT).await;
    quiet().await;
    assert!(answer.try_recv().is_err(), "still open");
    assert!(v.edits_of(card).is_empty());

    say(&call, reject).await;
    assert_eq!(answer.await, Ok(Answer::Permission(false)));
    v.until_said("Rejected.").await;
    assert_eq!(
        v.transcripts()
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>(),
        ["maybe later", "reject"]
    );
}

#[tokio::test(start_paused = true)]
async fn the_activity_follows_the_call() {
    let v = Voice::new().await;
    let call = v.call().await;
    say(&call, &fixture("prompt").audio()).await;
    v.h.backend.wait_for("prompt run the tests", 1).await;
    v.h.backend.turn_started(TurnSource::Prompt);
    v.activity("thinking").await;
    v.h.backend.text("Running them now.\n");
    v.activity("speaking").await;
    v.activity("thinking").await;
    v.h.backend.turn_ended(false);
    v.activity("on a call").await;
    let labels = v.labels();
    assert!(
        in_order(
            &labels,
            &[
                "on a call",
                "listening",
                "on a call",
                "thinking",
                "speaking",
                "thinking",
                "on a call"
            ]
        ),
        "{labels:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_owner_hangs_up_and_calls_again() {
    let v = Voice::new().await;
    let call = v.call().await;
    call.end(CallEndReason::Completed);
    v.h.idle().await;
    let again = v.call().await;
    assert_ne!(again.call_id, call.call_id);
    assert_eq!(v.phone.answered(), [call.call_id, again.call_id]);
    assert!(v.phone.declined().is_empty());
    v.activity("on a call").await;
}

#[tokio::test(start_paused = true)]
async fn call_events_during_the_answer_reach_the_line_after_it() {
    let v = Voice::new().await;
    let release = v.phone.hold_next_answer();
    let call_id = v.ring(v.h.messenger.owner_id());
    settle().await;
    v.h.messenger
        .inject(Event::Call(CallEvent::Taken { call_id }));
    v.h.messenger.inject(Event::Call(CallEvent::Taken {
        call_id: CallId::new(),
    }));
    settle().await;
    release.send(()).unwrap();
    let call = v.phone.wait_answered().await;
    call.wait_until("the held event", |call| !call.handled().is_empty())
        .await;
    assert_eq!(call.handled(), [CallEvent::Taken { call_id }]);
    v.h.messenger
        .inject(Event::Call(CallEvent::Taken { call_id }));
    call.wait_until("the next event", |call| call.handled().len() == 2)
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_transcript_is_masked_and_peeked_as_the_owner_s_text() {
    let v = Voice::new().await;
    let call = v.call().await;
    say(&call, &utterance(SECRET_HZ)).await;
    v.h.backend.wait_for("prompt my token", 1).await;
    let (speaker, text) = v.transcripts().pop().unwrap();
    assert_eq!(speaker, "owner");
    assert!(text.starts_with("my token="), "{text}");
    assert!(!text.contains("abc123def"), "{text}");
    let body = v.h.messenger.sent_rich().last().unwrap().1.clone();
    assert!(!body.contains("abc123def"), "{body}");
    let peek = peeked(&v.h).pop().unwrap();
    assert!(peek.text.starts_with("🎤 my token="), "{peek:?}");
    assert!(!peek.text.contains("abc123def"), "{peek:?}");
}

#[tokio::test(start_paused = true)]
async fn spoken_replies_and_background_messages_are_masked() {
    let v = Voice::new().await;
    let _call = v.call().await;
    let token = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    v.h.backend.turn_started(TurnSource::Prompt);
    v.h.backend.text(&format!(
        "I set api_key=sk-abc123def456 for you. Run `{token}` next.\n"
    ));
    v.h.backend.turn_ended(false);
    v.until_said("Run hidden next.").await;
    v.h.backend.emit(BackendEvent::AgentMessage {
        cursor: 1,
        label: "Deploy with password=hunter2xyz".into(),
        text: format!("The key is `token: {token}` now."),
        urgent: false,
    });
    v.until_said("The key is token: hidden now.").await;
    let spoken = v.spoken();
    assert!(spoken.contains(&"I set api_key=hidden for you.".to_owned()));
    assert!(spoken.contains(&"Deploy with password=hidden.".to_owned()));
    for secret in ["sk-abc123def456", token, "hunter2xyz"] {
        assert!(
            !spoken.iter().any(|said| said.contains(secret)),
            "{spoken:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_owner_speaking_during_a_code_block_keeps_the_code_silent() {
    let v = Voice::new().await;
    let call = v.call().await;
    v.h.backend.turn_started(TurnSource::Prompt);
    v.h.backend.text("Here is the fix:\n```\nlet first = 1;\n");
    v.until_said(CODE_NOTE).await;
    quiet().await;
    say(&call, &fixture("prompt").audio()).await;
    v.h.backend.wait_for("steer run the tests", 1).await;
    v.h.backend.text("let second = 2;\n```\nThat is all.\n");
    v.h.backend.turn_ended(false);
    v.until_said("That is all.").await;
    let spoken = v.spoken();
    assert!(
        !spoken.iter().any(|said| said.contains("second")),
        "{spoken:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_utterance_begun_before_a_card_opened_does_not_answer_it() {
    // The card opens while the owner says the word: its transcript comes after.
    let v = Voice::new().await;
    let call = v.call().await;
    let approve = fixture("approve").audio();
    let owner = saying(&call, approve.clone());
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut answer = v.h.backend.request(permission());
    let card = v.card("agent.approval@1").await;
    v.h.backend.wait_for("prompt approve", 1).await;
    owner.await.unwrap();
    v.until_said("Coda wants to use Bash. Approve or reject?")
        .await;
    quiet().await;
    assert!(answer.try_recv().is_err(), "still open");
    assert!(v.edits_of(card).is_empty());

    say(&call, &approve).await;
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
    v.until_said("Approved.").await;
    assert_eq!(v.h.count("prompt"), 1);
}

#[tokio::test(start_paused = true)]
async fn stopping_the_bridge_hangs_up() {
    let mut v = Voice::new().await;
    let call = v.call().await;
    v.h.stop().await.unwrap();
    call.wait_until("the hang-up", FakeCall::hung_up).await;
    assert_eq!(call.ended().unwrap().reason, CallEndReason::Completed);
}

// ── Answering and calling policies (PR 8) ──────────────────────────────────────────

const OWNER_HELLO: &str = "Hi, it's Coda in work.";
const BASH: &str = "Coda wants to use Bash. Approve or reject?";

fn ask() -> CallPolicy {
    CallPolicy {
        answer: AnswerMode::Ask,
        ..CallPolicy::default()
    }
}

/// Calls the owner after `after_s` seconds, at most every `interval_s` seconds.
fn notify(after_s: u64, interval_s: u64) -> CallPolicy {
    CallPolicy {
        notify: NotifyPolicy {
            after: Some(Duration::from_secs(after_s)),
            urgent: true,
            min_interval: Duration::from_secs(interval_s),
            quiet: QuietHours::default(),
        },
        ..CallPolicy::default()
    }
}

/// A local clock the test sets.
fn wall(time: &str) -> (Arc<Mutex<NaiveDateTime>>, crate::voice::policy::LocalTime) {
    let now = Arc::new(Mutex::new(
        NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M").unwrap(),
    ));
    let clock = now.clone();
    (now, Arc::new(move || *clock.lock().unwrap()))
}

impl Voice {
    /// The call on the control channel.
    fn call_info(&self) -> Option<(CallPhase, CallDirection)> {
        self.h
            .control_status()
            .call
            .map(|call| (call.phase, call.direction))
    }

    /// The `text` of the replies posted (not edits).
    fn replies(&self) -> Vec<String> {
        self.h.replies()
    }

    /// Waits (up to 15 minutes) until the bridge places its `count`th call.
    async fn dialed(&self, count: usize) -> FakeCall {
        for _ in 0..900 {
            if self.phone.dialed().len() >= count {
                return self.phone.wait_dialed().await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!("no call {count} was placed");
    }
}

#[tokio::test(start_paused = true)]
async fn ask_mode_answers_once_the_owner_accepts_the_card() {
    let mut v = Voice::with_policy(ask()).await;
    v.h.control_events();
    let call_id = v.ring(v.h.messenger.owner_id());
    let card = v.card("agent.question@1").await;
    let body =
        v.h.messenger
            .sent_rich()
            .into_iter()
            .find(|(id, _)| *id == card)
            .unwrap()
            .1;
    assert!(body.contains("Owner is calling Coda in work"), "{body}");
    assert!(
        body.contains("Answer") && body.contains("Decline"),
        "{body}"
    );
    settle().await;
    assert!(
        v.phone.answered().is_empty(),
        "not before the owner says so"
    );
    assert_eq!(
        v.call_info(),
        Some((CallPhase::Ringing, CallDirection::Incoming))
    );
    // Events about the call while it waits reach the line once answered.
    v.h.messenger
        .inject(Event::Call(CallEvent::Taken { call_id }));
    settle().await;

    v.h.messenger.answer(card, &["o1"]);
    let call = v.phone.wait_answered().await;
    assert_eq!(call.call_id, call_id);
    v.until_said(GREETING).await;
    assert_eq!(call.handled(), [CallEvent::Taken { call_id }]);
    assert!(
        v.edits_of(card).is_empty(),
        "the owner's answer shows on the card"
    );
    assert_eq!(
        v.call_info(),
        Some((CallPhase::InCall, CallDirection::Incoming))
    );
    let phases: Vec<_> = v
        .h
        .control_events()
        .into_iter()
        .filter_map(|event| match event {
            push_bridge_control::Event::CallChanged { call } => Some(call.map(|call| call.phase)),
            _ => None,
        })
        .collect();
    let mut folded = phases.clone();
    folded.dedup();
    assert!(
        in_order_of(
            &folded,
            &[
                Some(CallPhase::Ringing),
                Some(CallPhase::Connecting),
                Some(CallPhase::InCall)
            ]
        ),
        "{phases:?}"
    );
    call.end(CallEndReason::Completed);
    v.until("the end", || v.call_info().is_none()).await;
}

fn in_order_of<T: PartialEq>(seen: &[T], wanted: &[T]) -> bool {
    let mut rest = seen.iter();
    wanted.iter().all(|wanted| rest.any(|seen| seen == wanted))
}

#[tokio::test(start_paused = true)]
async fn ask_mode_declines_on_decline_and_closes_the_card_when_the_ring_ends() {
    let v = Voice::with_policy(ask()).await;
    let owner = v.h.messenger.owner_id();
    let declined = v.ring(owner);
    let card = v.card("agent.question@1").await;
    // A second call while one waits for the owner is busy.
    let second = v.ring(owner);
    v.phone.wait_declined(1).await;
    assert_eq!(v.phone.declined(), [(second, true)]);
    v.h.messenger.answer(card, &["o2"]);
    v.phone.wait_declined(2).await;
    assert_eq!(v.phone.declined()[1], (declined, false));
    assert!(v.edits_of(card).is_empty());
    assert!(v.phone.answered().is_empty());
    assert_eq!(v.call_info(), None);

    // The server ends the ring: the card says so and nothing is answered.
    let missed = v.ring(owner);
    settle().await;
    let card =
        v.h.messenger
            .sent_rich()
            .into_iter()
            .filter(|(_, body)| template(body) == "agent.question@1")
            .map(|(id, _)| id)
            .next_back()
            .unwrap();
    v.h.messenger.inject(Event::Call(CallEvent::Ended {
        call_id: missed,
        reason: CallEndReason::Missed,
        duration_s: None,
    }));
    v.until("the missed call", || !v.edits_of(card).is_empty())
        .await;
    assert_eq!(v.edits_of(card)[0].2, "📞 Missed call.");
    v.h.messenger.answer(card, &["o1"]);
    settle().await;
    assert!(v.phone.answered().is_empty(), "a late answer does nothing");
    assert_eq!(v.call_info(), None);
}

#[tokio::test(start_paused = true)]
async fn ask_mode_gives_up_after_the_ring_without_the_server() {
    let v = Voice::with_policy(ask()).await;
    let call_id = CallId::new();
    v.h.messenger.inject(Event::Call(CallEvent::Incoming {
        call_id,
        caller_id: v.h.messenger.owner_id(),
        offer_sdp: "offer".into(),
        expires_at_ms: 0,
    }));
    let card = v.card("agent.question@1").await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    v.phone.wait_declined(1).await;
    assert_eq!(v.phone.declined(), [(call_id, false)]);
    assert_eq!(v.edits_of(card)[0].2, "📞 Missed call.");
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_request_calls_the_owner_who_hears_it_and_approves_by_voice() {
    let v = Voice::with_policy(notify(120, 600)).await;
    let owner = v.h.messenger.owner_id();
    let answer = v.h.backend.request(permission());
    let _card = v.card("agent.approval@1").await;
    tokio::time::sleep(Duration::from_secs(100)).await;
    assert!(v.phone.dialed().is_empty(), "not before the wait");
    let call = v.dialed(1).await;
    assert_eq!(v.phone.dialed(), [owner]);
    assert_eq!(
        v.call_info(),
        Some((CallPhase::Ringing, CallDirection::Outgoing))
    );
    v.h.messenger
        .wait_until("waiting", |_| {
            v.h.messenger.activities().last().map(|(state, _)| *state)
                == Some(ActivityState::Waiting)
        })
        .await;
    call.pick_up();
    v.until_said(BASH).await;
    let spoken = v.spoken();
    assert_eq!(
        spoken,
        [
            OWNER_HELLO,
            "A request has waited 2 minutes for your answer.",
            BASH
        ]
    );
    assert_eq!(
        v.call_info(),
        Some((CallPhase::InCall, CallDirection::Outgoing))
    );
    quiet().await;
    say(&call, &fixture("approve").audio()).await;
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
    v.until_said("Approved.").await;
    call.end(CallEndReason::Completed);
    v.until("the end", || v.call_info().is_none()).await;
    assert!(
        !v.replies().iter().any(|reply| reply.contains("📞")),
        "{:?}",
        v.replies()
    );
}

#[tokio::test(start_paused = true)]
async fn a_missed_call_leaves_the_card_open_and_is_not_repeated() {
    let v = Voice::with_policy(notify(60, 600)).await;
    let answer = v.h.backend.request(permission());
    let card = v.card("agent.approval@1").await;
    let call = v.dialed(1).await;
    call.end(CallEndReason::Missed);
    v.h.reply_containing("📞 I called about the open request (no answer). It is still open above.")
        .await;
    assert!(v.edits_of(card).is_empty(), "the card is untouched");
    assert_eq!(v.call_info(), None);
    // The same request never calls again, even after the interval.
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(v.phone.dialed().len(), 1);
    // The card still takes the owner's answer.
    v.h.messenger.answer(card, &["approve"]);
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
}

#[tokio::test(start_paused = true)]
async fn declined_and_unreachable_calls_fall_back_to_the_card_too() {
    for (reason, why) in [
        (CallEndReason::Declined, "you declined"),
        (CallEndReason::Unreachable, "you could not be reached"),
    ] {
        let v = Voice::with_policy(notify(60, 600)).await;
        let _answer = v.h.backend.request(permission());
        let _card = v.card("agent.approval@1").await;
        let call = v.dialed(1).await;
        call.end(reason);
        v.h.reply_containing(&format!("({why})")).await;
    }
    // A phone that cannot place the call leaves the card as it is, quietly.
    let v = Voice::with_policy(notify(60, 600)).await;
    v.phone.fail_next_call(push_sdk::SdkError::Offline);
    let mut answer = v.h.backend.request(permission());
    let _card = v.card("agent.approval@1").await;
    tokio::time::sleep(Duration::from_secs(90)).await;
    assert!(v.phone.dialed().is_empty());
    assert_eq!(v.call_info(), None);
    assert!(answer.try_recv().is_err(), "still open");
}

#[tokio::test(start_paused = true)]
async fn calls_are_rate_limited() {
    let v = Voice::with_policy(notify(60, 600)).await;
    let _first = v.h.backend.request(permission());
    let _card = v.card("agent.approval@1").await;
    let call = v.dialed(1).await;
    let first_call = tokio::time::Instant::now();
    call.end(CallEndReason::Missed);
    v.h.reply_containing("(no answer)").await;
    // A second request is due a minute later, but the last call was less than ten
    // minutes ago.
    let _second = v.h.backend.request(RequestKind::Question {
        question: "Which region?".into(),
        options: vec!["EU".into(), "US".into()],
        multi_select: false,
        allow_free_text: false,
    });
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(v.phone.dialed().len(), 1, "too soon");
    let call = v.dialed(2).await;
    assert!(first_call.elapsed() >= Duration::from_secs(600));
    assert!(first_call.elapsed() < Duration::from_secs(602));
    call.pick_up();
    // The first card expired meanwhile: the oldest open one is spoken.
    v.until_said("Which region? Option one: EU. Option two: US.")
        .await;
}

#[tokio::test(start_paused = true)]
async fn no_call_in_the_do_not_disturb_hours() {
    let (now, clock) = wall("2026-11-02 23:30");
    let mut policy = notify(60, 600);
    policy.notify.quiet = QuietHours::parse(&["22:00-07:00".to_owned()], "dnd").unwrap();
    policy.local_time = Some(clock);
    let v = Voice::with_policy(policy).await;
    let _answer = v.h.backend.request(permission());
    let _card = v.card("agent.approval@1").await;
    tokio::time::sleep(Duration::from_secs(180)).await;
    assert!(v.phone.dialed().is_empty(), "quiet hours");
    *now.lock().unwrap() =
        NaiveDateTime::parse_from_str("2026-11-03 07:00", "%Y-%m-%d %H:%M").unwrap();
    let call = v.dialed(1).await;
    call.end(CallEndReason::Missed);
}

#[tokio::test(start_paused = true)]
async fn urgent_news_calls_and_ordinary_news_does_not() {
    let v = Voice::with_policy(notify(300, 600)).await;
    v.h.backend.emit(BackendEvent::AgentMessage {
        cursor: 1,
        label: "Nightly".into(),
        text: "All green.".into(),
        urgent: false,
    });
    v.h.reply_containing("All green.").await;
    settle().await;
    assert!(v.phone.dialed().is_empty());
    v.h.backend.emit(BackendEvent::AgentMessage {
        cursor: 2,
        label: "Build broken".into(),
        text: "The main branch fails to compile.".into(),
        urgent: true,
    });
    let call = v.dialed(1).await;
    call.pick_up();
    v.until_said("The main branch fails to compile.").await;
    assert_eq!(
        v.spoken(),
        [
            "Hi, it's Coda in work. I have something urgent.",
            "Build broken.",
            "The main branch fails to compile."
        ]
    );
    call.end(CallEndReason::Completed);

    // Off: urgent news stays in the chat.
    let mut policy = notify(300, 600);
    policy.notify.urgent = false;
    let off = Voice::with_policy(policy).await;
    off.h.backend.emit(BackendEvent::AgentMessage {
        cursor: 1,
        label: "Build broken".into(),
        text: "Again.".into(),
        urgent: true,
    });
    off.h.reply_containing("Again.").await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(off.phone.dialed().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_ringing_call_is_withdrawn_when_the_card_is_answered_on_screen() {
    let v = Voice::with_policy(notify(60, 600)).await;
    let answer = v.h.backend.request(permission());
    let card = v.card("agent.approval@1").await;
    let call = v.dialed(1).await;
    v.h.messenger.answer(card, &["reject"]);
    assert_eq!(answer.await, Ok(Answer::Permission(false)));
    call.wait_until("the cancel", FakeCall::hung_up).await;
    assert_eq!(call.ended().unwrap().reason, CallEndReason::Cancelled);
    v.until("the end", || v.call_info().is_none()).await;
    settle().await;
    assert!(!v.replies().iter().any(|reply| reply.contains("📞")));
}

#[tokio::test(start_paused = true)]
async fn the_tray_mutes_the_agent_and_hangs_up() {
    let v = Voice::new().await;
    let none = v.h.call(Action::Hangup).await.unwrap_err();
    assert_eq!(none.code, NO_CALL);
    assert_eq!(
        v.h.call(Action::Mute(true)).await.unwrap_err().code,
        NO_CALL
    );

    let call = v.call().await;
    assert_eq!(
        v.h.call(Action::Mute(true)).await,
        Ok(serde_json::json!({"muted": true}))
    );
    assert!(v.h.control_status().call.unwrap().muted);
    let spoken = v.spoken().len();
    v.h.backend.turn_started(TurnSource::Prompt);
    v.h.backend.text("Nobody hears this.\n");
    v.h.backend.turn_ended(false);
    quiet().await;
    assert_eq!(v.spoken().len(), spoken, "muted");
    assert_eq!(
        v.h.call(Action::Mute(false)).await,
        Ok(serde_json::json!({"muted": false}))
    );
    v.h.backend.turn_started(TurnSource::Prompt);
    v.h.backend.text("Now you hear me.\n");
    v.h.backend.turn_ended(false);
    v.until_said("Now you hear me.").await;

    assert_eq!(
        v.h.call(Action::Hangup).await,
        Ok(serde_json::json!({"hungUp": true}))
    );
    call.wait_until("the hang-up", FakeCall::hung_up).await;
    v.until("the end", || v.call_info().is_none()).await;
}

#[tokio::test(start_paused = true)]
async fn the_tray_cancels_a_call_ringing_the_owner_and_declines_an_ask() {
    let v = Voice::with_policy(notify(60, 600)).await;
    let _answer = v.h.backend.request(permission());
    let _card = v.card("agent.approval@1").await;
    let call = v.dialed(1).await;
    assert!(v.h.call(Action::Hangup).await.is_ok());
    call.wait_until("the cancel", FakeCall::hung_up).await;
    v.until("the end", || v.call_info().is_none()).await;
    settle().await;
    assert!(!v.replies().iter().any(|reply| reply.contains("📞")));

    let v = Voice::with_policy(ask()).await;
    let call_id = v.ring(v.h.messenger.owner_id());
    let card = v.card("agent.question@1").await;
    assert!(v.h.call(Action::Hangup).await.is_ok());
    v.phone.wait_declined(1).await;
    assert_eq!(v.phone.declined(), [(call_id, false)]);
    assert_eq!(v.edits_of(card)[0].2, "📞 Declined.");
}

// ── The voice adapter itself (PR 2) ─────────────────────────────────────────────────

/// The testkit adapter's word.
const WORD_HZ: f64 = 1000.0;
const WORD: &str = "run the tests";

fn adapter_script() -> Script {
    Script::with_words(vec![Word::new(WORD_HZ, WORD)])
}

/// The owner says [`WORD`] (the testkit's tone).
fn adapter_word() -> Vec<i16> {
    let mut audio = tones::silence(5);
    audio.extend(tones::tone(WORD_HZ, 20));
    audio.extend(tones::silence(25));
    audio
}

/// Adapters that start: `first`, then ones that are not found.
fn then_not_found(first: Vec<Plan>) -> Arc<ScriptedStarter> {
    ScriptedStarter::new(
        first
            .into_iter()
            .chain((0..50).map(|_| Plan::Fail(std::io::ErrorKind::NotFound))),
    )
}

/// A bridge whose speech runs in the voice adapter `supervisor` runs.
struct Adapted {
    v: Voice,
    supervisor: Supervisor,
    starter: Option<Arc<ScriptedStarter>>,
    flag: CallFlag,
    setup: AdapterSetup,
}

impl Adapted {
    async fn new(starter: Arc<ScriptedStarter>) -> Self {
        Self::with_policy(starter, CallPolicy::default()).await
    }

    /// With the answering and calling `policy`.
    async fn with_policy(starter: Arc<ScriptedStarter>, policy: CallPolicy) -> Self {
        let supervisor = Supervisor::with_starter(
            starter.clone(),
            BridgeHello::v1(env!("CARGO_PKG_VERSION"), "en-US"),
        );
        Self::build(supervisor, Some(starter), policy).await
    }

    async fn with(supervisor: Supervisor, starter: Option<Arc<ScriptedStarter>>) -> Self {
        Self::build(supervisor, starter, CallPolicy::default()).await
    }

    async fn build(
        supervisor: Supervisor,
        starter: Option<Arc<ScriptedStarter>>,
        policy: CallPolicy,
    ) -> Self {
        Self::build_on(Harness::named("Coda"), supervisor, starter, policy).await
    }

    /// With the echo backend and a ready-to-start adapter.
    async fn echo(starter: Arc<ScriptedStarter>) -> Self {
        let supervisor = Supervisor::with_starter(
            starter.clone(),
            BridgeHello::v1(env!("CARGO_PKG_VERSION"), "en-US"),
        );
        Self::build_on(
            Harness::echo(300),
            supervisor,
            Some(starter),
            CallPolicy::default(),
        )
        .await
    }

    async fn build_on(
        mut h: Harness,
        supervisor: Supervisor,
        starter: Option<Arc<ScriptedStarter>>,
        policy: CallPolicy,
    ) -> Self {
        let echo = h.is_echo();
        let phone = FakePhone::new();
        let flag: CallFlag = Arc::new(watch::channel(false).0);
        let setup = AdapterSetup::new(supervisor.clone(), "en-US");
        h.voice = Some(VoiceSetup {
            phone: phone.clone(),
            policy,
            adapter: setup.clone(),
        });
        h.call_flag = Some(flag.clone());
        h.start();
        if !echo {
            h.backend.wait_for("start", 1).await;
        }
        settle().await;
        Self {
            v: Voice {
                h,
                phone,
                starter: starter.clone(),
            },
            supervisor,
            starter,
            flag,
            setup,
        }
    }

    async fn ready(&self) {
        let mut current = self.supervisor.current();
        tokio::time::timeout(Duration::from_secs(30), current.wait_for(Option::is_some))
            .await
            .expect("the adapter gets ready")
            .unwrap();
    }

    /// The log of the `index`th adapter started, once it started.
    async fn log(&self, index: usize) -> Log {
        let starter = self.starter.as_ref().unwrap();
        self.v
            .until("the adapter", || starter.logs().len() > index)
            .await;
        starter.logs()[index].clone()
    }

    /// The owner calls; returns once the adapter took the call.
    async fn call(&self) -> FakeCall {
        self.v.ring(self.v.h.messenger.owner_id());
        let call = self.v.phone.wait_answered().await;
        let log = self.log(0).await;
        wait_log(&log, |event| {
            matches!(event, AdapterEvent::CallStarted { .. })
        })
        .await;
        call
    }
}

async fn wait_log(log: &Log, matches: impl Fn(&AdapterEvent) -> bool) -> AdapterEvent {
    tokio::time::timeout(Duration::from_secs(60), log.wait_for(matches))
        .await
        .expect("the adapter's event")
        .event
}

fn adapter_call_id(log: &Log) -> String {
    log.events()
        .into_iter()
        .find_map(|observed| match observed.event {
            AdapterEvent::CallStarted { call_id } => Some(call_id),
            _ => None,
        })
        .expect("call_started")
}

/// `speak`s of `chars` characters, interruptible or not.
fn speak(chars: usize, interruptible: bool) -> impl Fn(&AdapterEvent) -> bool {
    move |event| {
        matches!(event, AdapterEvent::Speak { chars: c, interruptible: i, .. }
            if *c == chars && *i == interruptible)
    }
}

#[tokio::test(start_paused = true)]
async fn with_a_ready_adapter_the_owner_s_call_is_answered_heard_and_spoken_to() {
    let a = Adapted::new(then_not_found(vec![Plan::Adapter(adapter_script())])).await;
    a.ready().await;
    let call = a.call().await;
    let log = a.log(0).await;
    wait_log(&log, speak(GREETING.chars().count(), false)).await;

    call.say_paced(&adapter_word()).await;
    a.v.h.backend.wait_for("prompt run the tests", 1).await;
    assert_eq!(a.v.transcripts(), [("owner".to_owned(), WORD.to_owned())]);

    a.v.h.backend.turn_started(TurnSource::Prompt);
    a.v.h.backend.text("All tests pass.\n");
    a.v.h.backend.turn_ended(false);
    wait_log(&log, speak("All tests pass.".chars().count(), true)).await;
    assert!(
        a.v.posted()
            .iter()
            .any(|text| text.contains("All tests pass.")),
        "the reply is in the chat too"
    );

    let adapter = a.v.h.control_status().adapter.unwrap();
    assert_eq!(adapter.state, "ready");
    assert_eq!(
        adapter.name.as_deref(),
        Some(push_voice_adapter_testkit::script::DEFAULT_NAME)
    );
    assert_eq!(
        adapter.capabilities.unwrap(),
        ["stt", "tts", "vad", "barge_in"]
    );
    call.end(CallEndReason::Completed);
    wait_log(
        &log,
        |event| matches!(event, AdapterEvent::CallEnded { reason, .. } if reason == "completed"),
    )
    .await;
    a.v.until("the end", || a.v.call_info().is_none()).await;
    let metrics = a.v.h.control_status().adapter.unwrap().metrics.unwrap();
    assert_eq!(metrics.calls, 1);
    assert!(metrics.frames_to_adapter >= 50, "{metrics:?}");
    assert!(metrics.frames_from_adapter > 0, "{metrics:?}");
    assert_eq!(metrics.speaks, 2);
    assert!(metrics.final_to_speak.is_some() && metrics.speak_to_audio.is_some());
}

#[tokio::test(start_paused = true)]
async fn the_echo_agent_speaks_each_final_transcript_back() {
    let a = Adapted::echo(then_not_found(vec![Plan::Adapter(adapter_script())])).await;
    a.ready().await;
    let call = a.call().await;
    let log = a.log(0).await;
    wait_log(&log, speak(GREETING.chars().count(), false)).await;

    call.say_paced(&adapter_word()).await;
    let echoed = format!("echo: {WORD}");
    wait_log(&log, speak(echoed.chars().count(), true)).await;
    assert_eq!(a.v.transcripts(), [("owner".to_owned(), WORD.to_owned())]);
    a.v.until("the echo in the chat", || {
        a.v.posted().iter().any(|text| text == &echoed)
    })
    .await;
    call.end(CallEndReason::Completed);
    a.v.until("the end", || a.v.call_info().is_none()).await;
}

#[tokio::test(start_paused = true)]
async fn a_lost_adapter_keeps_the_call_up_chat_only_until_it_is_back() {
    let a = Adapted::new(then_not_found(vec![
        Plan::Adapter(adapter_script()),
        Plan::Adapter(adapter_script()),
    ]))
    .await;
    a.ready().await;
    let call = a.call().await;
    let first = a.log(0).await;
    a.starter.as_ref().unwrap().crash();
    a.v.h.reply_containing(ADAPTER_LOST).await;
    assert!(call.ended().is_none(), "the call stays up");

    let second = a.log(1).await;
    wait_log(&second, |event| {
        matches!(event, AdapterEvent::CallStarted { .. })
    })
    .await;
    assert_eq!(
        adapter_call_id(&second),
        adapter_call_id(&first),
        "the same call goes on"
    );
    a.v.h.reply_containing(ADAPTER_BACK).await;
    let notes = a.v.replies();
    let lost = notes.iter().position(|note| note == ADAPTER_LOST).unwrap();
    let back = notes.iter().position(|note| note == ADAPTER_BACK).unwrap();
    assert!(lost < back, "{notes:?}");

    call.say_paced(&adapter_word()).await;
    a.v.h.backend.wait_for("prompt run the tests", 1).await;
    assert!(call.ended().is_none());
}

#[tokio::test(start_paused = true)]
async fn the_agent_s_replies_stay_in_the_chat_while_the_adapter_is_away() {
    let a = Adapted::new(then_not_found(vec![Plan::Adapter(adapter_script())])).await;
    a.ready().await;
    let call = a.call().await;
    let log = a.log(0).await;
    a.starter.as_ref().unwrap().crash();
    a.v.h.reply_containing(ADAPTER_LOST).await;
    let speaks = log.count(|event| matches!(event, AdapterEvent::Speak { .. }));
    a.v.h.backend.turn_started(TurnSource::Prompt);
    a.v.h.backend.text("Still working on it.\n");
    a.v.h.backend.turn_ended(false);
    a.v.until("the reply", || {
        a.v.posted()
            .iter()
            .any(|text| text.contains("Still working on it."))
    })
    .await;
    quiet().await;
    assert_eq!(
        log.count(|event| matches!(event, AdapterEvent::Speak { .. })),
        speaks
    );
    assert!(call.ended().is_none());
    assert!(!a.v.replies().iter().any(|note| note == ADAPTER_BACK));
}

#[tokio::test(start_paused = true)]
async fn without_a_ready_adapter_the_owner_s_calls_are_declined() {
    let a = Adapted::new(then_not_found(Vec::new())).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let first = a.v.ring(a.v.h.messenger.owner_id());
    a.v.phone.wait_declined(1).await;
    assert_eq!(a.v.phone.declined(), [(first, false)]);
    let adapter = a.v.h.control_status().adapter.unwrap();
    assert_eq!(adapter.state, "backoff");
    assert!(adapter.reason.is_some() && adapter.retry_at.is_some());

    let off = Adapted::with(Supervisor::off(), None).await;
    let second = off.v.ring(off.v.h.messenger.owner_id());
    off.v.phone.wait_declined(1).await;
    assert_eq!(off.v.phone.declined(), [(second, false)]);
    assert!(off.v.phone.answered().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_adapter_s_state_changes_go_to_the_control_channel() {
    let mut a = Adapted::new(then_not_found(vec![Plan::Adapter(adapter_script())])).await;
    a.ready().await;
    a.v.until("ready reported", || {
        a.v.h
            .control_status()
            .adapter
            .is_some_and(|adapter| adapter.state == "ready")
    })
    .await;
    let changes: Vec<_> =
        a.v.h
            .control_events()
            .into_iter()
            .filter_map(|event| match event {
                ControlEvent::AdapterChanged { adapter } => Some(adapter.unwrap()),
                _ => None,
            })
            .collect();
    let ready = changes.last().unwrap();
    assert_eq!(ready.state, "ready");
    assert!(changes.iter().all(|change| change.metrics.is_none()));
    a.starter.as_ref().unwrap().crash();
    a.v.until("backoff reported", || {
        a.v.h
            .control_status()
            .adapter
            .is_some_and(|adapter| adapter.state != "ready")
    })
    .await;
    assert!(a.v.h.control_events().iter().any(|event| matches!(
        event,
        ControlEvent::AdapterChanged { adapter: Some(adapter) } if adapter.state != "ready"
    )));
}

#[tokio::test(start_paused = true)]
async fn calls_are_advertised_while_the_adapter_listens_and_never_withdrawn_during_a_call() {
    let a = Adapted::new(then_not_found(vec![Plan::Adapter(adapter_script())])).await;
    let (sender, mut changes) = mpsc::unbounded_channel();
    tokio::spawn(advertise(
        a.supervisor.state(),
        a.flag.subscribe(),
        move |on| {
            let _ = sender.send((tokio::time::Instant::now(), on));
        },
    ));
    a.ready().await;
    let ready_at = tokio::time::Instant::now();
    let (at, on) = changes.recv().await.unwrap();
    assert!(on);
    assert!(
        at - ready_at >= Duration::from_millis(1_900),
        "after 2 s ready"
    );

    let call = a.call().await;
    assert!(*a.flag.borrow(), "a call is in progress");
    a.starter.as_ref().unwrap().crash();
    a.v.h.reply_containing(ADAPTER_LOST).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(changes.try_recv().is_err(), "not withdrawn during the call");

    call.end(CallEndReason::Completed);
    let (_, on) = tokio::time::timeout(Duration::from_secs(5), changes.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!on, "withdrawn once the call ended");
    assert!(!*a.flag.borrow());
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn an_adapter_that_cannot_speak_never_calls_the_owner() {
    let captured = Captured::default();
    let make = {
        let captured = captured.clone();
        move || captured.clone()
    };
    let _guard = tracing::subscriber::set_default(crate::logging::subscriber(
        make,
        tracing_subscriber::EnvFilter::new("info"),
    ));
    // A callsite another test thread was registering meanwhile may have cached `never`.
    std::thread::sleep(Duration::from_millis(50));
    tracing::callsite::rebuild_interest_cache();
    let mut script = adapter_script();
    script.capabilities.tts = false;
    let a =
        Adapted::with_policy(then_not_found(vec![Plan::Adapter(script)]), notify(60, 600)).await;
    a.ready().await;
    a.setup.set_advertised(true);
    let _answer = a.v.h.backend.request(permission());
    let _card = a.v.card("agent.approval@1").await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert!(a.v.phone.dialed().is_empty(), "a silent call");
    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let held: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("call_held") && line.contains("no_tts"))
        .collect();
    assert_eq!(held.len(), 1, "logged once: {logs}");
}

#[tokio::test(start_paused = true)]
async fn the_owner_is_called_through_the_adapter_only_while_calls_are_advertised() {
    let a = Adapted::with_policy(
        then_not_found(vec![Plan::Adapter(adapter_script())]),
        notify(60, 600),
    )
    .await;
    a.ready().await;
    let _answer = a.v.h.backend.request(permission());
    let _card = a.v.card("agent.approval@1").await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert!(a.v.phone.dialed().is_empty(), "not advertised");
    let setup = a.setup.clone();
    tokio::spawn(advertise(
        a.supervisor.state(),
        a.flag.subscribe(),
        move |on| setup.set_advertised(on),
    ));
    let call = a.v.dialed(1).await;
    assert_eq!(a.v.phone.dialed(), [a.v.h.messenger.owner_id()]);
    call.end(CallEndReason::Missed);
}

#[tokio::test(start_paused = true)]
async fn stopping_the_bridge_ends_the_adapter_s_call_as_a_shutdown() {
    let mut a = Adapted::new(then_not_found(vec![Plan::Adapter(adapter_script())])).await;
    a.ready().await;
    let call = a.call().await;
    let log = a.log(0).await;
    a.v.h.stop().await.unwrap();
    let ended = wait_log(&log, |event| {
        matches!(event, AdapterEvent::CallEnded { .. })
    })
    .await;
    assert_eq!(
        ended,
        AdapterEvent::CallEnded {
            call_id: adapter_call_id(&log),
            reason: "shutdown".into(),
        }
    );
    call.wait_until("the hang-up", FakeCall::hung_up).await;
}

/// A core that does not run, taking calls on a [`FakePhone`] and keeping `flag`.
struct IdleCore {
    core: super::Core<crate::messenger::TestMessenger>,
    _events: mpsc::Receiver<Event>,
    _dir: crate::test_dir::TestDir,
}

impl IdleCore {
    fn new(flag: &CallFlag) -> Self {
        let dir = crate::test_dir::TestDir::new();
        let (messenger, events) = crate::messenger::TestMessenger::new();
        let (sender, _) = mpsc::unbounded_channel();
        let backend = Arc::new(crate::test_backend::TestBackend::new("Coda", sender));
        let state =
            Arc::new(crate::state::StateFile::load(&dir.path().join("state.json")).unwrap());
        let (internal, _) = mpsc::unbounded_channel();
        let owner = messenger.owner_id();
        let conversation = messenger.conversation();
        let mut core = super::Core::new(
            messenger,
            backend,
            state,
            super::BridgeOptions {
                roots: vec![dir.path().to_path_buf()],
                default_folder: dir.path().to_path_buf(),
                default_mode: crate::backend::PermissionMode::Default,
            },
            internal,
            owner,
            conversation,
        );
        core.voice = Some(VoiceSetup {
            phone: FakePhone::new(),
            policy: CallPolicy::default(),
            adapter: AdapterSetup::new(Supervisor::off(), "en-US"),
        });
        core.call_flag = Some(flag.clone());
        Self {
            core,
            _events: events,
            _dir: dir,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_call_flag_is_up_before_the_phone_answers_asks_or_dials() {
    let flag: CallFlag = Arc::new(watch::channel(false).0);
    let mut idle = IdleCore::new(&flag);
    let core = &mut idle.core;
    core.answer_call(CallId::new(), "offer", Vec::new());
    assert!(*flag.borrow(), "answering");

    core.answering = None;
    core.sync_call_flag();
    assert!(!*flag.borrow());
    core.ask(CallId::new(), "offer".into(), i64::MAX);
    assert!(*flag.borrow(), "asking");

    core.asking = None;
    core.sync_call_flag();
    assert!(!*flag.borrow());
    core.dial(super::calling::Purpose::Request).await;
    assert!(*flag.borrow(), "dialing");
}

#[tokio::test(start_paused = true)]
async fn the_call_flag_holds_through_the_shutdown() {
    let mut a = Adapted::new(then_not_found(vec![Plan::Adapter(adapter_script())])).await;
    a.ready().await;
    let _call = a.call().await;
    assert!(*a.flag.borrow());
    let mut flag = a.flag.subscribe();
    flag.mark_unchanged();
    a.v.h.stop().await.unwrap();
    assert!(*a.flag.borrow(), "still up once the bridge stopped");
    assert!(
        !flag.has_changed().unwrap(),
        "never lowered during the shutdown"
    );
}
