//! Voice calls in the core, against a [`FakePhone`], fake speech and the recorded
//! fixtures in `tests/fixtures/voice`, in paused time (the fake speaker plays one frame
//! per 20 ms of tokio's clock).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use push_sdk::{ActivityState, CallEndReason, CallEvent, CallId, Event, UserId};

use super::tests::{Harness, peeked, permission, settle};
use crate::backend::{Answer, BackendEvent, RequestKind, TurnSource};
use crate::messenger::{param, template};
use crate::voice::VoiceSetup;
use crate::voice::confirm::APPROVE_OR_REJECT;
use crate::voice::fake::{FakeStt, FakeTts, Fixture, silence, tone};
use crate::voice::phone::{FakeCall, FakePhone};
use crate::voice::provider::Stt;
use crate::voice::speech::CODE_NOTE;
use crate::voice::vad::VadConfig;

const GREETING: &str = "Connected to Coda in work.";
const FIXTURES: [&str; 5] = ["prompt", "barge_in", "approve", "option_two", "ambiguous"];
/// A word no fixture uses, with a secret in it.
const SECRET_HZ: f64 = 2200.0;
const SECRET_WORDS: &str = "my token=abc123def";

fn fixture(name: &str) -> Fixture {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/voice")
        .join(format!("{name}.json"));
    Fixture::load(&path).unwrap()
}

/// Hears every fixture's words, and [`SECRET_WORDS`].
fn stt() -> FakeStt {
    let mut words: Vec<(f64, String)> = FIXTURES
        .iter()
        .flat_map(|name| fixture(name).words)
        .map(|word| (word.hz, word.text))
        .collect();
    words.push((SECRET_HZ, SECRET_WORDS.to_owned()));
    FakeStt::new(words)
}

/// The owner says one word at `hz`.
fn utterance(hz: f64) -> Vec<i16> {
    let mut audio = silence(200);
    audio.extend(tone(hz, 900, 8000.0));
    audio.extend(silence(1000));
    audio
}

/// A bridge with voice, its phone and its voice.
struct Voice {
    h: Harness,
    phone: Arc<FakePhone>,
    tts: Arc<FakeTts>,
}

impl Voice {
    async fn new() -> Self {
        Self::with_stt(Some(stt())).await
    }

    async fn with_stt(stt: Option<FakeStt>) -> Self {
        let mut h = Harness::named("Coda");
        let phone = FakePhone::new();
        let tts = Arc::new(FakeTts::new());
        h.voice = Some(VoiceSetup {
            phone: phone.clone(),
            stt: stt.map(|stt| Arc::new(stt) as Arc<dyn Stt>),
            tts: Some(tts.clone()),
            vad: VadConfig::default(),
        });
        h.start();
        h.backend.wait_for("start", 1).await;
        settle().await;
        Self { h, phone, tts }
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

    /// How often `text` was spoken.
    fn said(&self, text: &str) -> usize {
        self.tts
            .spoken()
            .iter()
            .filter(|said| *said == text)
            .count()
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
        panic!(
            "timed out waiting for {what}; spoken {:?}",
            self.tts.spoken()
        );
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

/// Lets what the agent says play out (FakeTts speaks at most 3 s a sentence).
async fn quiet() {
    tokio::time::sleep(Duration::from_secs(3)).await;
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
    assert_eq!(v.tts.spoken(), [GREETING]);
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

    let off = Voice::with_stt(None).await;
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
    call.say(&prompt.audio()).await;
    v.h.backend.wait_for("prompt run the tests", 1).await;
    assert_eq!(
        v.transcripts(),
        [("owner".to_owned(), "run the tests".to_owned())]
    );
    let peek = peeked(&v.h);
    assert_eq!(peek.last().unwrap().text, "🎤 run the tests");

    v.h.backend.turn_started(TurnSource::Prompt);
    settle().await;
    call.say(&prompt.audio()).await;
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
        v.tts.spoken()[1..],
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
    // The STT takes 2 s, so the reply goes on before the owner's words arrive.
    let v = Voice::with_stt(Some(stt().with_delay(Duration::from_secs(2)))).await;
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
    call.say(&fixture("barge_in").audio()).await;
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

    let spoken = v.tts.spoken();
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
}

#[tokio::test(start_paused = true)]
async fn reply_text_handled_before_the_barge_in_event_is_not_spoken() {
    let v = Voice::with_stt(Some(stt().with_delay(Duration::from_secs(2)))).await;
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
    call.say(&fixture("barge_in").audio()).await;
    v.h.backend
        .wait_for("steer stop, use the other file", 1)
        .await;
    v.h.backend.text("Then I switch files.\n");
    v.h.backend.turn_ended(false);
    v.until_said("Then I switch files.").await;
    let spoken = v.tts.spoken();
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

    call.say(&fixture("approve").audio()).await;
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
    call.say(&fixture("option_two").audio()).await;
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
    call.say(maybe).await;
    v.until_said(APPROVE_OR_REJECT).await;
    quiet().await;
    assert!(answer.try_recv().is_err(), "still open");
    assert!(v.edits_of(card).is_empty());

    call.say(reject).await;
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
    call.say(&fixture("prompt").audio()).await;
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
    call.say(&utterance(SECRET_HZ)).await;
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
    });
    v.until_said("The key is token: hidden now.").await;
    let spoken = v.tts.spoken();
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
    call.say(&fixture("prompt").audio()).await;
    v.h.backend.wait_for("steer run the tests", 1).await;
    v.h.backend.text("let second = 2;\n```\nThat is all.\n");
    v.h.backend.turn_ended(false);
    v.until_said("That is all.").await;
    let spoken = v.tts.spoken();
    assert!(
        !spoken.iter().any(|said| said.contains("second")),
        "{spoken:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_utterance_begun_before_a_card_opened_does_not_answer_it() {
    // The STT takes 2 s: the card opens while the utterance is transcribed.
    let v = Voice::with_stt(Some(stt().with_delay(Duration::from_secs(2)))).await;
    let call = v.call().await;
    let approve = fixture("approve").audio();
    call.say(&approve).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut answer = v.h.backend.request(permission());
    let card = v.card("agent.approval@1").await;
    v.h.backend.wait_for("prompt approve", 1).await;
    v.until_said("Coda wants to use Bash. Approve or reject?")
        .await;
    quiet().await;
    assert!(answer.try_recv().is_err(), "still open");
    assert!(v.edits_of(card).is_empty());

    call.say(&approve).await;
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
