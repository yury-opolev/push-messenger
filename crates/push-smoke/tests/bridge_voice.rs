//! The agent bridge answers its owner's call end to end (docs/agent-bridge.md, "Voice
//! calls"): the whole stack in this process with calls and agent-key sign-in, the owner on
//! an SDK client placing the call with push-sdk's media helper, and the agent enrolled with
//! a software key running the bridge as `push-agent-bridge run` does (an `SdkMessenger`,
//! `MediaPhone`), with a scripted backend and its speech in the testkit's scripted voice
//! adapter (spawned; `Agent::start`). Over real Opus
//! media the owner hears the greeting and the spoken replies, its recorded fixtures become
//! a prompt, a steer that cuts the reply short (barge-in) and a voice approval; a stranger's
//! call never reaches the bridge, and the owner hangs up. A second test runs the answering
//! and calling policies: with `calls.answer` `ask` the owner's call waits for the card's
//! "Answer"; a card left unanswered makes the bridge call the owner, who hears the summary
//! and approves by voice; a call the owner lets ring out leaves the card open.
//!
//! Opt in with `PUSH_BRIDGE_VOICE=1` (otherwise the test passes without running): it needs
//! Docker. CI runs it in the `bridge-voice` job.

mod bridge_voice_common;
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge_voice_common::*;
use common::Stack;
use push_bridge::backend::{Answer, RequestKind, TurnSource};
use push_bridge::messenger::{param, template};
use push_bridge::voice::policy::{CallPolicy, NotifyPolicy};
use push_sdk::media::{EndCause, MediaCall, MediaOptions, MediaState, Role};
use push_sdk::{ActivityState, CallEndReason, CallErrorCode, ContactState, MessageKind};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_answers_its_owners_call() {
    if !enabled() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let started = Instant::now();
    let (stack, containers) = stack().await;
    step(&format!(
        "the stack is up in {} ms",
        started.elapsed().as_millis()
    ));
    tokio::time::timeout(Duration::from_secs(300), scenario(&stack))
        .await
        .expect("the scenario finishes within 300 s");
    stack.stop().await;
    drop(containers);
    step(&format!("done in {} ms", started.elapsed().as_millis()));
}

async fn scenario(stack: &Stack) {
    let dir = scratch();
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 2)
        .await
        .expect("two verified accounts");
    let mut owner = Person::start(stack, &dir, "owner", &accounts[0]).await;
    let mut stranger = Person::start(stack, &dir, "stranger", &accounts[1]).await;
    owner.client.request_contact(stranger.user).await.unwrap();
    let accepted = stranger.client.accept_contact(owner.user).await.unwrap();
    assert_eq!(accepted.state, ContactState::Accepted);

    let http = reqwest::Client::new();
    let token = access_token(&http, stack, &accounts[0]).await;
    let created = post(
        &http,
        stack,
        "/v1/agents",
        Some(&token),
        json!({"handle": "voice_agent", "display_name": "Voice agent"}),
    )
    .await;
    let code = created["enrollment_code"].as_str().unwrap();
    let agent = Agent::start(stack, &dir, code, CallPolicy::default()).await;
    owner.watch(agent.user);
    stranger.watch(agent.user);
    within("the backend starts", agent.backend.wait_for("start", 1)).await;
    step("the agent enrolled and its bridge runs");

    // (a) The owner calls; the bridge answers and greets.
    let placed = Instant::now();
    let (owner_call, incoming) = call(&mut owner, agent.user).await.expect("the call starts");
    let ear = Ear::default();
    let listening = ear.listen(incoming);
    within("the owner's media connects", owner_call.connected())
        .await
        .expect("the call connects");
    step(&format!(
        "the bridge answered; media connected {} ms after the call",
        placed.elapsed().as_millis()
    ));
    ear.voice("the greeting", placed).await;
    ear.quiet("the end of the greeting", placed).await;
    assert_eq!(agent.phone.answered(), [owner_call.call_id()]);

    // (b) The owner asks: a transcript, and a prompt.
    let seen = owner.message_count();
    say(&owner_call, &fixture("prompt").audio()).await;
    within(
        "the prompt",
        agent.backend.wait_for("prompt run the tests", 1),
    )
    .await;
    let transcript = owner
        .message("the transcript", seen, |seen| {
            template(seen.body()) == "agent.transcript@1"
        })
        .await;
    assert_eq!(transcript.message.kind, MessageKind::Rich);
    assert_eq!(
        param(transcript.body(), "text").as_deref(),
        Some("run the tests")
    );
    step("the prompt reached the backend; the owner got the transcript");

    // (c) The reply is spoken and posted.
    let seen = owner.message_count();
    let replied = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    agent.backend.text("All tests pass.\n");
    agent.backend.turn_ended(false);
    ear.voice("the reply", replied).await;
    owner
        .message("the reply", seen, |seen| {
            template(seen.body()) == "agent.reply@1"
                && param(seen.body(), "text").is_some_and(|text| text.contains("All tests pass."))
        })
        .await;
    ear.quiet("the end of the reply", replied).await;

    // (d) Barge-in: the owner talks over a long reply, which stops; the words steer.
    let long = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    let sentence = "This sentence is one of many in a long reply that keeps going.";
    agent
        .backend
        .text(&format!("{}\n", vec![sentence; 30].join(" ")));
    ear.voice("the long reply", long).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let barge_in = Instant::now();
    say(&owner_call, &fixture("barge_in").audio()).await;
    within(
        "the steer",
        agent.backend.wait_for("steer stop, use the other file", 1),
    )
    .await;
    let before = ear.windows(barge_in - Duration::from_millis(500), barge_in);
    assert!(
        before.iter().any(|(_, level)| *level > HEARD),
        "the agent spoke right before the barge-in: {before:?}"
    );
    // The owner's words start 200 ms in; the agent stops within 1.5 s of them.
    let cut = barge_in + Duration::from_millis(1_700);
    let quiet_until = barge_in + Duration::from_secs(4);
    tokio::time::sleep_until(tokio::time::Instant::from_std(quiet_until)).await;
    let after = ear.windows(cut, quiet_until);
    let ended = ear
        .windows(barge_in, quiet_until)
        .into_iter()
        .rfind(|(_, level)| *level > QUIET)
        .map_or(0, |(at, _)| {
            (at.duration_since(barge_in) + Duration::from_millis(100)).as_millis()
        });
    step(&format!(
        "barge-in: the agent's voice ended {ended} ms after the owner's audio started (its words 200 ms in)"
    ));
    assert!(
        after.iter().all(|(_, level)| *level < QUIET),
        "the agent stopped speaking: {after:?}"
    );
    agent.backend.turn_ended(false);

    // (e) A permission card, approved by voice.
    let seen = owner.message_count();
    let asked = Instant::now();
    agent.backend.turn_started(TurnSource::Prompt);
    let answer = agent.backend.request(RequestKind::Permission {
        tool: "Bash".into(),
        preview: "cargo test".into(),
    });
    let card = owner
        .message("the card", seen, |seen| {
            !seen.edited && template(seen.body()) == "agent.approval@1"
        })
        .await;
    ear.voice("the card's question", asked).await;
    ear.quiet("the end of the question", asked).await;
    let approved = Instant::now();
    say(&owner_call, &fixture("approve").audio()).await;
    let answer = within("the voice answer", answer).await;
    assert_eq!(answer, Ok(Answer::Permission(true)));
    let card_id = card.message.client_msg_id;
    let edit = owner
        .message("the card's edit", seen, |seen| {
            seen.edited && seen.message.client_msg_id == card_id
        })
        .await;
    assert_eq!(
        param(edit.body(), "text").as_deref(),
        Some("🎤 Answered by voice: Approve.")
    );
    step("the card was approved by voice and edited");
    agent.backend.turn_ended(false);
    ear.voice("the confirmation", approved).await;
    ear.quiet("the end of the confirmation", approved).await;

    // (f) A stranger (the owner's contact, not the agent's) is refused by the server.
    let refused = match call(&mut stranger, agent.user).await {
        Err(error) => {
            step(&format!("the stranger's call was refused at once: {error}"));
            None
        }
        Ok((stranger_call, _)) => {
            let ended = within("the stranger's call ends", stranger_call.on_ended()).await;
            step(&format!("the stranger's call ended: {ended:?}"));
            assert_eq!(ended.cause, EndCause::Refused(CallErrorCode::NotContact));
            Some(stranger_call.call_id())
        }
    };
    assert_eq!(agent.phone.answered(), [owner_call.call_id()]);
    assert_eq!(agent.phone.declined(), []);
    if let Some(stranger_call) = refused {
        assert_ne!(stranger_call, owner_call.call_id());
    }

    // (g) The owner hangs up: both sides end.
    let hung_up = Instant::now();
    let ended = within("the owner's hang-up", owner_call.hangup()).await;
    assert_eq!(
        (ended.reason, ended.cause),
        (CallEndReason::Completed, EndCause::Local)
    );
    within("the owner's stream ends", listening)
        .await
        .expect("the listener ran");
    until("the agent is idle again", || {
        let log = owner.log.lock().unwrap();
        log.activity
            .iter()
            .any(|(at, state, _)| *at >= hung_up && *state == ActivityState::Idle)
            .then_some(())
    })
    .await;
    step("the owner hung up; the bridge's call ended");

    let Agent {
        client,
        stop,
        run,
        backend,
        phone,
        adapter,
        advertiser,
        ..
    } = agent;
    let _ = stop.send(());
    within("the bridge stops", run)
        .await
        .expect("the bridge ran")
        .expect("the bridge stopped cleanly");
    stop_adapter(adapter, advertiser).await;
    assert!(backend.calls().iter().any(|call| call == "stop"));
    // The phone holds a handle of the client: only the last one closes it.
    drop((owner_call, phone));
    client.close().await.unwrap();
    owner.client.close().await.unwrap();
    stranger.client.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridge_asks_before_answering_and_calls_its_owner() {
    if !enabled() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let started = Instant::now();
    let (mut datastores, containers) = common::calls_datastores().await;
    // A short ring: the missed call ends after 10 s.
    datastores
        .message_svc
        .push(("PUSH_CALLS_RING_SECONDS".to_owned(), "10".to_owned()));
    datastores
        .gateway
        .push(("PUSH_CALLS_ATTEMPTS_PER_HOUR".to_owned(), "1000".to_owned()));
    for (name, value) in [
        ("PUSH_TURN_MODE", "stun-only"),
        ("PUSH_TURN_STUN_URLS", "stun:127.0.0.1:3478"),
        ("PUSH_AUTH_PROVIDERS", "password,agent-key"),
        ("PUSH_AUTH_AGENT_KEY_AUDIENCE", AUDIENCE),
        ("PUSH_AGENTS_PER_USER", "2"),
    ] {
        datastores.auth.push((name.to_owned(), value.to_owned()));
    }
    let stack = Stack::start(datastores).await;
    step(&format!(
        "the stack is up in {} ms",
        started.elapsed().as_millis()
    ));
    tokio::time::timeout(Duration::from_secs(300), policies(&stack))
        .await
        .expect("the scenario finishes within 300 s");
    stack.stop().await;
    drop(containers);
    step(&format!("done in {} ms", started.elapsed().as_millis()));
}

async fn policies(stack: &Stack) {
    let dir = scratch();
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 1)
        .await
        .expect("a verified account");
    let mut owner = Person::start(stack, &dir, "owner", &accounts[0]).await;
    let mut line = Line2::new(owner.calls.take().expect("the call events are free"));
    let http = reqwest::Client::new();
    let token = access_token(&http, stack, &accounts[0]).await;
    let created = post(
        &http,
        stack,
        "/v1/agents",
        Some(&token),
        json!({"handle": "calling_agent", "display_name": "Calling agent"}),
    )
    .await;
    let code = created["enrollment_code"].as_str().unwrap();
    // Ask before answering; call about a card after 3 s, at most once a second.
    let policy = CallPolicy {
        answer: push_bridge::config::AnswerMode::Ask,
        notify: NotifyPolicy {
            after: Some(Duration::from_secs(3)),
            min_interval: Duration::from_secs(1),
            ..NotifyPolicy::default()
        },
        local_time: None,
    };
    let agent = Agent::start(stack, &dir, code, policy).await;
    owner.watch(agent.user);
    within("the backend starts", agent.backend.wait_for("start", 1)).await;
    step("the agent enrolled and its bridge runs (ask, notify_by_call after 3 s)");

    // (a) Ask: the owner's call waits for "Answer" on the card, then is answered.
    let seen = owner.message_count();
    let placed = Instant::now();
    let (owner_call, incoming) = call_with(&owner, &line, agent.user).await;
    let ask = owner
        .message("the ask card", seen, |seen| {
            !seen.edited && template(seen.body()) == "agent.question@1"
        })
        .await;
    assert!(
        ask.body().contains("Owner is calling Test in work"),
        "{}",
        ask.body()
    );
    assert!(agent.phone.answered().is_empty(), "not answered yet");
    assert_eq!(owner_call.state(), MediaState::Connecting);
    step("the owner's call waits for the ask card");
    answer_card(&owner, &ask, &["o1"]);
    let ear = Ear::default();
    let listening = ear.listen(incoming);
    within("the owner's media connects", owner_call.connected())
        .await
        .expect("the call connects");
    ear.voice("the greeting", placed).await;
    ear.quiet("the end of the greeting", placed).await;
    assert_eq!(agent.phone.answered(), [owner_call.call_id()]);
    step("answered after the owner accepted the card");
    let hung_up = Instant::now();
    within("the owner's hang-up", owner_call.hangup()).await;
    within("the owner's stream ends", listening)
        .await
        .expect("the listener ran");
    line.clear();
    until("the agent is idle again", || {
        let log = owner.log.lock().unwrap();
        log.activity
            .iter()
            .any(|(at, state, _)| *at >= hung_up && *state == ActivityState::Idle)
            .then_some(())
    })
    .await;

    // (b) A card unanswered for 3 s: the bridge calls; the owner answers, hears the
    // summary and the question, and approves by voice.
    let seen = owner.message_count();
    agent.backend.turn_started(TurnSource::Prompt);
    let answer = agent.backend.request(RequestKind::Permission {
        tool: "Bash".into(),
        preview: "cargo test".into(),
    });
    let card = owner
        .message("the card", seen, |seen| {
            !seen.edited && template(seen.body()) == "agent.approval@1"
        })
        .await;
    let opened = Instant::now();
    let (call_id, offer_sdp) = line.ringing("the bridge's call").await;
    let rang = opened.elapsed();
    step(&format!(
        "the bridge called {} ms after the card",
        rang.as_millis()
    ));
    assert!(rang >= Duration::from_secs(2), "{rang:?}");
    assert_eq!(*agent.phone.dialed.lock().unwrap(), [owner.user]);
    let servers = owner.client.calls().ice_servers().await.unwrap();
    let mut answered = MediaCall::start(
        &owner.client.calls(),
        &servers,
        Role::Callee { call_id, offer_sdp },
        MediaOptions::default(),
    )
    .await
    .expect("the owner answers");
    let ear = Ear::default();
    let listening = ear.listen(answered.incoming().expect("the callee's stream"));
    let answered = Arc::new(answered);
    line.set(answered.clone());
    let picked_up = Instant::now();
    within("the media connects", answered.connected())
        .await
        .expect("the call connects");
    ear.voice("the summary", picked_up).await;
    ear.quiet("the end of the summary and question", picked_up)
        .await;
    say(&answered, &fixture("approve").audio()).await;
    let answer = within("the voice answer", answer).await;
    assert_eq!(answer, Ok(Answer::Permission(true)));
    let card_id = card.message.client_msg_id;
    let edit = owner
        .message("the card's edit", seen, |seen| {
            seen.edited && seen.message.client_msg_id == card_id
        })
        .await;
    assert_eq!(
        param(edit.body(), "text").as_deref(),
        Some("🎤 Answered by voice: Approve.")
    );
    step("the owner heard the summary and approved by voice");
    agent.backend.turn_ended(false);
    within("the owner's hang-up", answered.hangup()).await;
    within("the owner's stream ends", listening)
        .await
        .expect("the listener ran");
    line.clear();

    // (c) The next call is missed: the card stays open and takes the owner's answer.
    let seen = owner.message_count();
    agent.backend.turn_started(TurnSource::Prompt);
    let answer = agent.backend.request(RequestKind::Permission {
        tool: "Bash".into(),
        preview: "cargo build".into(),
    });
    let card = owner
        .message("the second card", seen, |seen| {
            !seen.edited && template(seen.body()) == "agent.approval@1"
        })
        .await;
    let (missed, _) = line.ringing("the bridge's second call").await;
    step("the owner lets the bridge's call ring out");
    owner
        .message("the missed-call note", seen, |seen| {
            template(seen.body()) == "agent.reply@1"
                && param(seen.body(), "text").is_some_and(|text| {
                    text.contains("I called about the open request (no answer)")
                })
        })
        .await;
    assert_ne!(missed, call_id);
    let card_id = card.message.client_msg_id;
    assert!(
        !owner
            .log
            .lock()
            .unwrap()
            .messages
            .iter()
            .any(|seen| seen.edited && seen.message.client_msg_id == card_id),
        "the card is untouched"
    );
    answer_card(&owner, &card, &["approve"]);
    let answer = within("the card's answer", answer).await;
    assert_eq!(answer, Ok(Answer::Permission(true)));
    step("the missed call fell back to the card");
    agent.backend.turn_ended(false);
    assert_eq!(agent.phone.dialed.lock().unwrap().len(), 2);

    let Agent {
        client,
        stop,
        run,
        phone,
        adapter,
        advertiser,
        ..
    } = agent;
    let _ = stop.send(());
    within("the bridge stops", run)
        .await
        .expect("the bridge ran")
        .expect("the bridge stopped cleanly");
    stop_adapter(adapter, advertiser).await;
    drop((owner_call, answered, phone, line));
    client.close().await.unwrap();
    owner.client.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
