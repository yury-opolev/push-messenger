//! Bridge actions in the core (spec 2026-11-09 proactive agents §7.1, §7.3) against a
//! [`TestBackend`](crate::test_backend::TestBackend) and a scripted
//! [`TestMessenger`](crate::messenger::TestMessenger), in paused time: each action's
//! happy path, the server's refusals as stable codes, waiting actions kept and finished
//! on the owner's decision, and the catch-up at start-up and after a reconnect.

use std::time::Duration;

use push_sdk::{
    ConnectionState, ConversationId, Event, OutgoingAction, OutgoingKind, OutgoingState, SdkError,
    UserId,
};

use super::tests::{Harness, settle};
use crate::actions::{Action, ActionCode, ActionRequest, ActionResult, Target};
use crate::backend::{BackendEvent, TurnSource};
use crate::messenger::{Call, Messenger, param, server_error, template};
use crate::state::{PendingAction, PendingKind};

/// The backend asks for `action`; its result.
async fn act(h: &Harness, action: Action) -> ActionResult {
    let (request, result) = ActionRequest::new(action);
    h.backend.emit(BackendEvent::Action(request));
    tokio::time::timeout(Duration::from_secs(60), result)
        .await
        .expect("answered in time")
        .expect("an answer")
}

fn waiting(id: &str) -> SdkError {
    SdkError::Server {
        code: "waiting_for_owner".into(),
        message: "waits for the owner".into(),
        retryable: false,
        retry_after_s: None,
        action_id: Some(id.into()),
    }
}

fn send(to: &str, text: &str) -> Action {
    Action::SendMessage {
        to: to.parse().unwrap(),
        text: text.into(),
    }
}

/// One of the agent's actions as the server reports it.
fn decided(
    h: &Harness,
    id: &str,
    kind: OutgoingKind,
    state: OutgoingState,
    conversation: Option<ConversationId>,
) -> OutgoingAction {
    OutgoingAction {
        action_id: id.into(),
        agent: h.messenger.me(),
        kind,
        target: None,
        conversation,
        title: None,
        members: Vec::new(),
        state,
        error: None,
        created_at_ms: 0,
        resolved_at_ms: Some(1),
    }
}

/// The RICH reply texts sent into `conversation`.
fn rich_in(h: &Harness, conversation: ConversationId) -> Vec<String> {
    h.messenger
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::SendRich {
                conversation: sent,
                body,
                ..
            } if sent == conversation && template(&body).starts_with("agent.reply@") => {
                param(&body, "text")
            }
            _ => None,
        })
        .collect()
}

fn texts_in(h: &Harness, conversation: ConversationId) -> Vec<String> {
    h.messenger
        .texts()
        .into_iter()
        .filter(|(sent, _)| *sent == conversation)
        .map(|(_, text)| text)
        .collect()
}

fn pending(h: &Harness) -> Vec<(String, PendingKind)> {
    h.state
        .get()
        .pending_actions
        .into_iter()
        .map(|(id, pending)| (id, pending.kind))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_contact_request_goes_to_the_looked_up_user() {
    let h = Harness::started().await;
    let bob = h.messenger.add_user("bob", false);
    let result = act(
        &h,
        Action::RequestContact {
            handle: "@bob".into(),
        },
    )
    .await;
    assert_eq!(result.code, ActionCode::Done, "{result:?}");
    assert_eq!(result.detail, "contact request sent to @bob");
    let calls = h.messenger.calls();
    assert!(calls.contains(&Call::Lookup {
        handle: "bob".into()
    }));
    assert!(calls.contains(&Call::RequestContact { user: bob }));

    let unknown = act(
        &h,
        Action::RequestContact {
            handle: "nobody".into(),
        },
    )
    .await;
    assert_eq!(
        (unknown.code, unknown.detail.as_str()),
        (ActionCode::NotFound, "no user @nobody")
    );
}

#[tokio::test(start_paused = true)]
async fn the_server_s_refusals_answer_stable_codes() {
    let h = Harness::started().await;
    h.messenger.add_user("bob", false);
    let request = || Action::RequestContact {
        handle: "bob".into(),
    };
    h.messenger
        .fail_next("request_contact", server_error("not_allowed_by_owner"));
    assert_eq!(act(&h, request()).await.code, ActionCode::NotAllowedByOwner);
    h.messenger.fail_next(
        "request_contact",
        SdkError::Server {
            code: "rate_limited".into(),
            message: "slow down".into(),
            retryable: true,
            retry_after_s: Some(30),
            action_id: None,
        },
    );
    let limited = act(&h, request()).await;
    assert_eq!(
        (limited.code, limited.retry_after_s),
        (ActionCode::RateLimited, Some(30))
    );
    h.messenger
        .fail_next("open_direct", server_error("not_contact"));
    assert_eq!(
        act(&h, send("@bob", "hi")).await.code,
        ActionCode::NotContact
    );
    h.messenger.fail_next("lookup", SdkError::Offline);
    assert_eq!(act(&h, request()).await.code, ActionCode::Unavailable);
    assert!(pending(&h).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_waiting_request_is_kept_and_its_outcome_told_to_the_owner() {
    let h = Harness::started().await;
    h.messenger.add_user("bob", false);
    h.messenger.fail_next("request_contact", waiting("a-1"));
    let result = act(
        &h,
        Action::RequestContact {
            handle: "bob".into(),
        },
    )
    .await;
    assert_eq!(
        (result.code, result.action_id.as_deref()),
        (ActionCode::WaitingForOwner, Some("a-1"))
    );
    assert_eq!(
        pending(&h),
        [(
            "a-1".to_owned(),
            PendingKind::Request {
                handle: "bob".into()
            }
        )]
    );
    h.messenger.inject(Event::OutgoingActionChanged(decided(
        &h,
        "a-1",
        OutgoingKind::Request,
        OutgoingState::Approved,
        None,
    )));
    h.reply_containing("Owner approved: contact request to @bob sent.")
        .await;
    assert!(pending(&h).is_empty());
}

#[tokio::test(start_paused = true)]
async fn messages_to_the_owner_are_replies_in_the_owner_s_chat() {
    let h = Harness::started().await;
    let result = act(&h, send("owner", "the build is green")).await;
    assert_eq!(result.code, ActionCode::Done);
    assert_eq!(
        result.conversation,
        Some(h.messenger.conversation().to_string())
    );
    // The owner's own handle is the owner too.
    let by_handle = act(&h, send("@test-owner", "and deployed")).await;
    assert_eq!(by_handle.code, ActionCode::Done);
    assert_eq!(
        rich_in(&h, h.messenger.conversation()),
        ["the build is green", "and deployed"]
    );
    assert!(
        !h.messenger
            .calls()
            .iter()
            .any(|call| matches!(call, Call::OpenDirect { .. }))
    );
}

#[tokio::test(start_paused = true)]
async fn agents_get_plain_text_and_people_a_rich_reply() {
    let h = Harness::started().await;
    let bob = h.messenger.add_user("bob", false);
    let helper = h.messenger.add_user("helper", true);
    // Inside a turn as well as outside one.
    h.backend.turn_started(TurnSource::Prompt);
    let to_bob = act(&h, send("@bob", "hello Bob")).await;
    let to_helper = act(&h, send("@helper", "hello agent")).await;
    let (bob_chat, helper_chat) = (
        h.messenger.conversation_with(bob),
        h.messenger.conversation_with(helper),
    );
    assert_eq!(
        (to_bob.code, to_bob.detail.as_str()),
        (ActionCode::Done, "sent to @bob")
    );
    assert_eq!(to_bob.conversation, Some(bob_chat.to_string()));
    assert_eq!(rich_in(&h, bob_chat), ["hello Bob"]);
    assert!(texts_in(&h, bob_chat).is_empty());
    assert_eq!(to_helper.code, ActionCode::Done);
    assert_eq!(texts_in(&h, helper_chat), ["hello agent"]);
    assert!(rich_in(&h, helper_chat).is_empty());
    let opened: Vec<_> = h
        .messenger
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::OpenDirect { peer } => Some(peer),
            _ => None,
        })
        .collect();
    assert_eq!(opened, [bob, helper]);
}

#[tokio::test(start_paused = true)]
async fn a_held_message_is_sent_once_the_owner_approves_the_chat() {
    let h = Harness::started().await;
    let bob = h.messenger.add_user("bob", false);
    h.messenger.fail_next("open_direct", waiting("a-2"));
    let result = act(&h, send("@bob", "shall we meet?")).await;
    assert_eq!(
        (result.code, result.action_id.as_deref()),
        (ActionCode::WaitingForOwner, Some("a-2"))
    );
    assert_eq!(
        pending(&h),
        [(
            "a-2".to_owned(),
            PendingKind::Send {
                target: bob.to_string(),
                handle: "bob".into(),
                text: "shall we meet?".into(),
                agent: false,
            }
        )]
    );
    let chat = ConversationId::new();
    // Someone else's action, and this one still waiting, change nothing.
    let mut other = decided(&h, "a-2", OutgoingKind::Chat, OutgoingState::Approved, None);
    other.agent = UserId::new();
    h.messenger.inject(Event::OutgoingActionChanged(other));
    h.messenger.inject(Event::OutgoingActionChanged(decided(
        &h,
        "a-2",
        OutgoingKind::Chat,
        OutgoingState::Waiting,
        None,
    )));
    settle().await;
    assert_eq!(pending(&h).len(), 1);
    h.messenger.inject(Event::OutgoingActionChanged(decided(
        &h,
        "a-2",
        OutgoingKind::Chat,
        OutgoingState::Approved,
        Some(chat),
    )));
    h.reply_containing("Owner approved: message to @bob sent.")
        .await;
    assert_eq!(rich_in(&h, chat), ["shall we meet?"]);
    assert!(pending(&h).is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_approval_without_the_chat_opens_it_and_agents_get_plain_text() {
    let h = Harness::started().await;
    let helper = h.messenger.add_user("helper", true);
    h.messenger.fail_next("open_direct", waiting("a-3"));
    act(&h, send("@helper", "ping")).await;
    h.messenger.inject(Event::OutgoingActionChanged(decided(
        &h,
        "a-3",
        OutgoingKind::Chat,
        OutgoingState::Approved,
        None,
    )));
    h.reply_containing("Owner approved: message to @helper sent.")
        .await;
    assert_eq!(
        texts_in(&h, h.messenger.conversation_with(helper)),
        ["ping"]
    );
}

#[tokio::test(start_paused = true)]
async fn declined_and_failed_actions_are_dropped_and_told() {
    let h = Harness::started().await;
    h.messenger.add_user("bob", false);
    h.messenger.add_user("carol", false);
    h.messenger.fail_next("open_direct", waiting("a-4"));
    act(&h, send("@bob", "hi")).await;
    h.messenger.fail_next("open_direct", waiting("a-5"));
    act(&h, send("@carol", "hi")).await;
    assert_eq!(pending(&h).len(), 2);
    h.messenger.inject(Event::OutgoingActionChanged(decided(
        &h,
        "a-4",
        OutgoingKind::Chat,
        OutgoingState::OwnerDeclined,
        None,
    )));
    let mut failed = decided(&h, "a-5", OutgoingKind::Chat, OutgoingState::Failed, None);
    failed.error = Some("not_contact".into());
    h.messenger.inject(Event::OutgoingActionChanged(failed));
    h.reply_containing("Failed: message to @carol (not_contact).")
        .await;
    assert!(
        h.replies()
            .contains(&"Owner declined: message to @bob.".to_owned())
    );
    assert!(pending(&h).is_empty());
    assert!(h.messenger.texts().is_empty());
    assert_eq!(rich_in(&h, h.messenger.conversation()).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn messages_go_into_groups_the_agent_is_a_member_of() {
    let h = Harness::started().await;
    let group = h.messenger.add_group();
    let result = act(&h, send(&format!("group:{group}"), "hello all")).await;
    assert_eq!(result.code, ActionCode::Done);
    assert_eq!(result.conversation, Some(format!("group:{group}")));
    assert_eq!(rich_in(&h, group), ["hello all"]);
    let stranger = ConversationId::new();
    let refused = act(&h, send(&format!("group:{stranger}"), "hi")).await;
    assert_eq!(refused.code, ActionCode::NotFound);
    assert!(rich_in(&h, stranger).is_empty());
}

#[tokio::test(start_paused = true)]
async fn groups_are_created_and_contacts_invited() {
    let h = Harness::started().await;
    let alice = h.messenger.add_user("alice", false);
    let bob = h.messenger.add_user("bob", true);
    let created = act(
        &h,
        Action::CreateGroup {
            name: "Release crew".into(),
            members: vec!["@alice".into(), "bob".into()],
        },
    )
    .await;
    assert_eq!(created.code, ActionCode::Done, "{created:?}");
    assert!(h.messenger.calls().contains(&Call::CreateGroup {
        title: "Release crew".into(),
        members: vec![alice, bob],
    }));
    let group = created.conversation.expect("the group");
    assert!(group.starts_with("group:"));
    let id: ConversationId = group["group:".len()..].parse().unwrap();

    let carol = h.messenger.add_user("carol", false);
    let invited = act(
        &h,
        Action::Invite {
            group: group.clone(),
            member: "@carol".into(),
        },
    )
    .await;
    assert_eq!(
        (invited.code, invited.detail.as_str()),
        (ActionCode::Done, &*format!("@carol added to {group}"))
    );
    assert!(h.messenger.calls().contains(&Call::AddMembers {
        group: id,
        users: vec![carol],
    }));

    let missing = act(
        &h,
        Action::CreateGroup {
            name: "x".into(),
            members: vec!["alice".into(), "nobody".into()],
        },
    )
    .await;
    assert_eq!(missing.code, ActionCode::NotFound);

    h.messenger
        .fail_next("create_group", server_error("not_available"));
    let off = act(
        &h,
        Action::CreateGroup {
            name: "x".into(),
            members: vec!["alice".into()],
        },
    )
    .await;
    assert_eq!(
        (off.code, off.detail.as_str()),
        (
            ActionCode::Unavailable,
            "groups are not enabled on this server"
        )
    );
    h.messenger.fail_next("add_members", waiting("a-6"));
    let asked = act(
        &h,
        Action::Invite {
            group: group.clone(),
            member: "carol".into(),
        },
    )
    .await;
    assert_eq!(asked.code, ActionCode::WaitingForOwner);
    assert_eq!(
        pending(&h),
        [(
            "a-6".to_owned(),
            PendingKind::Invite {
                group: group.clone(),
                handle: "carol".into()
            }
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn notes_to_the_owner_are_titled_with_the_backend_s_name() {
    let h = Harness::started().await;
    let result = act(
        &h,
        Action::NotifyOwner {
            text: "Tests are green.".into(),
        },
    )
    .await;
    assert_eq!(result.code, ActionCode::Done);
    let (_, body) = h.messenger.sent_rich().pop().expect("the note");
    assert_eq!(param(&body, "title").as_deref(), Some("Test"));
    assert_eq!(param(&body, "text").as_deref(), Some("Tests are green."));
}

#[tokio::test(start_paused = true)]
async fn calls_need_voice_and_never_reach_other_agents() {
    let h = Harness::started().await;
    h.messenger.add_user("bob", false);
    h.messenger.add_user("helper", true);
    let call = |to: &str| Action::Call {
        to: to.parse::<Target>().unwrap(),
        reason: Some("the deploy is stuck".into()),
    };
    let owner = act(&h, call("owner")).await;
    assert_eq!(
        (owner.code, owner.detail.as_str()),
        (ActionCode::Unavailable, "voice calls are not set up")
    );
    assert_eq!(act(&h, call("@helper")).await.code, ActionCode::NotAllowed);
    let person = act(&h, call("@bob")).await;
    assert_eq!(person.code, ActionCode::Failed);
    assert!(person.detail.starts_with("not implemented"));
    assert_eq!(act(&h, call("nobody")).await.code, ActionCode::NotFound);
}

#[tokio::test(start_paused = true)]
async fn outgoing_lists_the_agent_s_actions() {
    let h = Harness::started().await;
    let bob = h.messenger.add_user("bob", false);
    let mut chat = decided(&h, "a-7", OutgoingKind::Chat, OutgoingState::Waiting, None);
    chat.target = Some(bob);
    let mut group = decided(&h, "a-8", OutgoingKind::Group, OutgoingState::Failed, None);
    group.title = Some("crew".into());
    group.error = Some("not_contact".into());
    h.messenger.set_outgoing(vec![chat, group]);
    let result = act(&h, Action::Outgoing).await;
    assert_eq!(result.code, ActionCode::Done);
    assert_eq!(
        result.detail,
        "1 waiting\nchat @bob: waiting\ngroup \"crew\": failed (not_contact)"
    );
    h.messenger
        .fail_next("outgoing_actions", server_error("not_allowed"));
    assert_eq!(act(&h, Action::Outgoing).await.code, ActionCode::NotAllowed);
}

#[tokio::test(start_paused = true)]
async fn actions_decided_while_away_are_settled_at_start_and_after_a_reconnect() {
    let mut h = Harness::named("Test");
    let now = super::Clock::new().now_ms();
    let keep = |kind: PendingKind, age_ms: i64| PendingAction {
        kind,
        created_at_ms: now - age_ms,
    };
    let send = |handle: &str| PendingKind::Send {
        target: UserId::new().to_string(),
        handle: handle.into(),
        text: format!("hi {handle}"),
        agent: false,
    };
    h.state
        .update(|state| {
            state
                .pending_actions
                .insert("approved".into(), keep(send("bob"), 1_000));
            state
                .pending_actions
                .insert("waiting".into(), keep(send("carol"), 1_000));
            state
                .pending_actions
                .insert("old".into(), keep(send("dave"), 49 * 3_600 * 1_000));
        })
        .unwrap();
    let chat = ConversationId::new();
    h.messenger.set_outgoing(vec![
        decided(
            &h,
            "approved",
            OutgoingKind::Chat,
            OutgoingState::Approved,
            Some(chat),
        ),
        decided(
            &h,
            "waiting",
            OutgoingKind::Chat,
            OutgoingState::Waiting,
            None,
        ),
    ]);
    h.start();
    h.reply_containing("Owner approved: message to @bob sent.")
        .await;
    assert_eq!(rich_in(&h, chat), ["hi bob"]);
    let kept: Vec<_> = pending(&h).into_iter().map(|(id, _)| id).collect();
    assert_eq!(kept, ["waiting"]);

    // Declined while the connection was down: the list after the reconnect tells.
    h.messenger.set_outgoing(vec![decided(
        &h,
        "waiting",
        OutgoingKind::Chat,
        OutgoingState::OwnerDeclined,
        None,
    )]);
    h.messenger
        .inject(Event::ConnectionStateChanged(ConnectionState::Connected));
    h.messenger
        .inject(Event::ConnectionStateChanged(ConnectionState::Connecting));
    h.messenger
        .inject(Event::ConnectionStateChanged(ConnectionState::Connected));
    h.reply_containing("Owner declined: message to @carol.")
        .await;
    assert!(pending(&h).is_empty());
    let lists = h
        .messenger
        .calls()
        .iter()
        .filter(|call| **call == Call::OutgoingActions)
        .count();
    assert_eq!(lists, 2);
    h.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn nothing_kept_lists_nothing() {
    let h = Harness::started().await;
    settle().await;
    assert!(!h.messenger.calls().contains(&Call::OutgoingActions));
}
