//! Bridge actions (spec 2026-11-09 proactive agents §7.1): what a backend asks the bridge
//! to do on its own, at any time: a contact request, a message to the owner, a contact or
//! a group, a note to the owner, a group or an invite to one, a call, or the status of its
//! actions waiting for the owner. Every backend uses the same [`Action`] and gets the same
//! [`ActionResult`]: Echo through its commands ([`parse_command`]), Coda through tools.
//!
//! The result codes ([`ActionCode`]) are stable: tools pass them on as they are.

use std::fmt;
use std::str::FromStr;

use push_sdk::{ConversationId, SdkError};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// The prefix of a group in a [`Target`] and in results: `group:<id>`.
pub const GROUP_PREFIX: &str = "group:";

/// Who a message or a call is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Target {
    /// The agent's owner, in the owner's chat.
    Owner,
    /// A user by handle (without the `@`).
    Handle(String),
    /// A group the agent is a member of.
    Group(ConversationId),
}

impl FromStr for Target {
    type Err = String;

    /// `owner`, `@handle` or `handle`, or `group:<id>`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("owner") {
            return Ok(Self::Owner);
        }
        if text.starts_with(GROUP_PREFIX) {
            return parse_group(text)
                .map(Self::Group)
                .ok_or_else(|| format!("not a group id: {text}"));
        }
        handle(text)
            .map(Self::Handle)
            .ok_or_else(|| format!("not a handle: {text}"))
    }
}

impl TryFrom<String> for Target {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<Target> for String {
    fn from(target: Target) -> Self {
        target.to_string()
    }
}

impl fmt::Display for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owner => formatter.write_str("owner"),
            Self::Handle(handle) => write!(formatter, "@{handle}"),
            Self::Group(group) => write!(formatter, "{GROUP_PREFIX}{group}"),
        }
    }
}

/// A handle without its `@`; `None` when empty or not one word.
pub fn handle(text: &str) -> Option<String> {
    let handle = text.trim().trim_start_matches('@');
    (!handle.is_empty() && !handle.contains(char::is_whitespace) && !handle.contains('@'))
        .then(|| handle.to_owned())
}

/// A group id, `group:<id>` or the bare id.
pub fn parse_group(text: &str) -> Option<ConversationId> {
    let text = text.trim();
    text.strip_prefix(GROUP_PREFIX).unwrap_or(text).parse().ok()
}

/// `group:<id>`.
pub fn group_name(group: ConversationId) -> String {
    format!("{GROUP_PREFIX}{group}")
}

/// What a backend asks the bridge to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Asks the user with this handle to become a contact.
    RequestContact { handle: String },
    /// Sends `text` to the owner, a user (opening the direct chat if needed) or a group.
    SendMessage { to: Target, text: String },
    /// A labelled note in the owner's chat.
    NotifyOwner { text: String },
    /// Creates a group of accepted contacts (by handle).
    CreateGroup { name: String, members: Vec<String> },
    /// Adds an accepted contact (by handle) to a group (`group:<id>`).
    Invite { group: String, member: String },
    /// Calls the owner or a contact; `reason` is said when they pick up.
    Call {
        to: Target,
        #[serde(default)]
        reason: Option<String>,
    },
    /// The agent's actions waiting for the owner, and those resolved lately.
    Outgoing,
}

impl Action {
    /// The action's name, as in logs and tools (never its arguments).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RequestContact { .. } => "request_contact",
            Self::SendMessage { .. } => "send_message",
            Self::NotifyOwner { .. } => "notify_owner",
            Self::CreateGroup { .. } => "create_group",
            Self::Invite { .. } => "invite",
            Self::Call { .. } => "call",
            Self::Outgoing => "outgoing",
        }
    }
}

/// A backend's action and the way its result goes back. `Debug` shows the action's kind
/// only (never a text).
pub struct ActionRequest {
    pub action: Action,
    pub reply: oneshot::Sender<ActionResult>,
}

impl fmt::Debug for ActionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ActionRequest({})", self.action.kind())
    }
}

impl ActionRequest {
    /// A request and the receiver its result arrives on.
    pub fn new(action: Action) -> (Self, oneshot::Receiver<ActionResult>) {
        let (reply, result) = oneshot::channel();
        (Self { action, reply }, result)
    }
}

/// How an action ended (spec §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCode {
    /// Carried out.
    Done,
    /// The owner decides; the bridge finishes it on approval.
    WaitingForOwner,
    /// The owner's policy is `off`.
    NotAllowedByOwner,
    /// A limit; `retry_after_s` says when to try again.
    RateLimited,
    /// The loop guard paused that conversation.
    Paused,
    /// Unknown handle or group.
    NotFound,
    /// Not an accepted contact.
    NotContact,
    /// The rules of agents forbid it.
    NotAllowed,
    /// No voice, a call already running, or the server unreachable.
    Unavailable,
    /// Anything else (the detail says what).
    Failed,
}

impl ActionCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::WaitingForOwner => "waiting_for_owner",
            Self::NotAllowedByOwner => "not_allowed_by_owner",
            Self::RateLimited => "rate_limited",
            Self::Paused => "paused",
            Self::NotFound => "not_found",
            Self::NotContact => "not_contact",
            Self::NotAllowed => "not_allowed",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        }
    }
}

/// An action's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    pub code: ActionCode,
    /// What happened, for the backend (never empty).
    pub detail: String,
    /// `waiting_for_owner`: the action's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// `rate_limited`: when to try again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u32>,
    /// The conversation the action used or made: a direct chat's id, or `group:<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
}

impl ActionResult {
    pub fn new(code: ActionCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            action_id: None,
            retry_after_s: None,
            conversation: None,
        }
    }

    pub fn done(detail: impl Into<String>) -> Self {
        Self::new(ActionCode::Done, detail)
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Self::new(ActionCode::Failed, detail)
    }

    pub fn with_conversation(mut self, conversation: impl Into<String>) -> Self {
        self.conversation = Some(conversation.into());
        self
    }

    /// The server's refusal as a result: its code mapped to a stable one.
    pub fn from_sdk(error: &SdkError) -> Self {
        let message = match error {
            SdkError::Server { message, .. } if !message.is_empty() => message.clone(),
            other => other.to_string(),
        };
        let code = match error.code() {
            "waiting_for_owner" => {
                let mut result = Self::new(
                    ActionCode::WaitingForOwner,
                    "waiting for the owner's approval",
                );
                result.action_id = error.action_id().map(str::to_owned);
                return result;
            }
            "not_allowed_by_owner" => {
                return Self::new(
                    ActionCode::NotAllowedByOwner,
                    "the owner does not allow this",
                );
            }
            "rate_limited" => {
                let mut result = Self::new(ActionCode::RateLimited, "rate limited");
                if let SdkError::Server { retry_after_s, .. } = error {
                    result.retry_after_s = *retry_after_s;
                }
                return result;
            }
            "not_found"
            | "user_not_found"
            | "target_not_found"
            | "conversation_not_found"
            | "not_a_group" => ActionCode::NotFound,
            "not_contact" | "blocked" => ActionCode::NotContact,
            "not_allowed" | "not_admin" | "not_member" | "owner_decides" => ActionCode::NotAllowed,
            "offline" | "disconnected" | "network" | "connection_busy" | "unavailable" => {
                return Self::new(ActionCode::Unavailable, "the server cannot be reached");
            }
            "not_available" => ActionCode::Unavailable,
            _ => ActionCode::Failed,
        };
        Self::new(code, format!("{}: {message}", error.code()))
    }

    /// One line for a person: `done: …`, `waiting for owner (action …)`, `rate limited,
    /// retry in N s`, `not found: …`.
    pub fn summary(&self) -> String {
        match self.code {
            ActionCode::Done => format!("done: {}", self.detail),
            ActionCode::WaitingForOwner => match &self.action_id {
                Some(id) => format!("waiting for owner (action {id})"),
                None => "waiting for owner".to_owned(),
            },
            ActionCode::NotAllowedByOwner => "not allowed by owner".to_owned(),
            ActionCode::RateLimited => match self.retry_after_s {
                Some(seconds) => format!("rate limited, retry in {seconds} s"),
                None => "rate limited".to_owned(),
            },
            code => format!("{}: {}", code.as_str().replace('_', " "), self.detail),
        }
    }
}

/// An echo command (spec §7.6) in the owner's text: `None` when the first word is no
/// command keyword; `Err(usage)` when it is but the rest does not fit.
pub fn parse_command(text: &str) -> Option<Result<Action, &'static str>> {
    let text = text.trim();
    let (word, rest) = split_word(text);
    let word = word.to_ascii_lowercase();
    let parsed = match word.as_str() {
        "request" => parse_request(rest).ok_or("request @handle"),
        "send" => parse_send(rest).ok_or("send @handle|owner|group:<id> <text>"),
        "notify" => (!rest.is_empty())
            .then(|| Action::NotifyOwner {
                text: rest.to_owned(),
            })
            .ok_or("notify <text>"),
        "group" => parse_group_command(rest).ok_or("group <name|\"quoted name\"> @a @b …"),
        "invite" => parse_invite(rest).ok_or("invite group:<id> @handle"),
        "call" => parse_call(rest).ok_or("call @handle|owner [reason]"),
        "outgoing" => rest
            .is_empty()
            .then_some(Action::Outgoing)
            .ok_or("outgoing"),
        _ => return None,
    };
    Some(parsed)
}

fn split_word(text: &str) -> (&str, &str) {
    match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (text, ""),
    }
}

fn parse_request(rest: &str) -> Option<Action> {
    let (word, more) = split_word(rest);
    (more.is_empty() && word.starts_with('@'))
        .then(|| handle(word))
        .flatten()
        .map(|handle| Action::RequestContact { handle })
}

/// A handle written with its `@`, `owner`, or `group:<id>`.
fn command_target(word: &str) -> Option<Target> {
    if word.starts_with('@') || word.eq_ignore_ascii_case("owner") || word.starts_with(GROUP_PREFIX)
    {
        word.parse().ok()
    } else {
        None
    }
}

fn parse_send(rest: &str) -> Option<Action> {
    let (word, text) = split_word(rest);
    let to = command_target(word)?;
    (!text.is_empty()).then(|| Action::SendMessage {
        to,
        text: text.to_owned(),
    })
}

fn parse_group_command(rest: &str) -> Option<Action> {
    let (name, members) = if let Some(quoted) = rest.strip_prefix('"') {
        let (name, members) = quoted.split_once('"')?;
        (name.trim(), members.trim())
    } else {
        split_word(rest)
    };
    if name.is_empty() {
        return None;
    }
    let members: Vec<String> = members
        .split_whitespace()
        .map(|word| word.starts_with('@').then(|| handle(word)).flatten())
        .collect::<Option<_>>()?;
    (!members.is_empty()).then(|| Action::CreateGroup {
        name: name.to_owned(),
        members,
    })
}

fn parse_invite(rest: &str) -> Option<Action> {
    let (group, member) = split_word(rest);
    let (member, more) = split_word(member);
    let group = parse_group(group).filter(|_| group.starts_with(GROUP_PREFIX))?;
    (more.is_empty() && member.starts_with('@'))
        .then(|| handle(member))
        .flatten()
        .map(|member| Action::Invite {
            group: group_name(group),
            member,
        })
}

fn parse_call(rest: &str) -> Option<Action> {
    let (word, reason) = split_word(rest);
    let to = command_target(word).filter(|to| !matches!(to, Target::Group(_)))?;
    Some(Action::Call {
        to,
        reason: (!reason.is_empty()).then(|| reason.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Action {
        parse_command(text).expect("a command").expect("valid")
    }

    fn usage(text: &str) -> &'static str {
        parse_command(text).expect("a command").expect_err("usage")
    }

    #[test]
    fn commands_parse_with_any_case_keyword() {
        let group = ConversationId::new();
        assert_eq!(
            parsed("Request @bob"),
            Action::RequestContact {
                handle: "bob".into()
            }
        );
        assert_eq!(
            parsed("send @bob hi  there"),
            Action::SendMessage {
                to: Target::Handle("bob".into()),
                text: "hi  there".into()
            }
        );
        assert_eq!(
            parsed("SEND owner hello"),
            Action::SendMessage {
                to: Target::Owner,
                text: "hello".into()
            }
        );
        assert_eq!(
            parsed(&format!("send group:{group} hey")),
            Action::SendMessage {
                to: Target::Group(group),
                text: "hey".into()
            }
        );
        assert_eq!(
            parsed("notify the build is green"),
            Action::NotifyOwner {
                text: "the build is green".into()
            }
        );
        assert_eq!(
            parsed("group team @a @b"),
            Action::CreateGroup {
                name: "team".into(),
                members: vec!["a".into(), "b".into()]
            }
        );
        assert_eq!(
            parsed("group \"Release crew\" @a"),
            Action::CreateGroup {
                name: "Release crew".into(),
                members: vec!["a".into()]
            }
        );
        assert_eq!(
            parsed(&format!("invite group:{group} @carol")),
            Action::Invite {
                group: format!("group:{group}"),
                member: "carol".into()
            }
        );
        assert_eq!(
            parsed("call @bob the deploy is stuck"),
            Action::Call {
                to: Target::Handle("bob".into()),
                reason: Some("the deploy is stuck".into())
            }
        );
        assert_eq!(
            parsed("call owner"),
            Action::Call {
                to: Target::Owner,
                reason: None
            }
        );
        assert_eq!(parsed("outgoing"), Action::Outgoing);
    }

    #[test]
    fn malformed_commands_answer_their_usage() {
        assert_eq!(usage("request"), "request @handle");
        assert_eq!(usage("request bob"), "request @handle");
        assert_eq!(usage("send @bob"), "send @handle|owner|group:<id> <text>");
        assert_eq!(usage("send bob hi"), "send @handle|owner|group:<id> <text>");
        assert_eq!(
            usage("send group:nope hi"),
            "send @handle|owner|group:<id> <text>"
        );
        assert_eq!(usage("notify"), "notify <text>");
        assert_eq!(usage("group team"), "group <name|\"quoted name\"> @a @b …");
        assert_eq!(
            usage("group \"open"),
            "group <name|\"quoted name\"> @a @b …"
        );
        assert_eq!(
            usage("group team @a b"),
            "group <name|\"quoted name\"> @a @b …"
        );
        assert_eq!(usage("invite @bob"), "invite group:<id> @handle");
        assert_eq!(usage("call"), "call @handle|owner [reason]");
        assert_eq!(usage("outgoing now"), "outgoing");
    }

    #[test]
    fn other_texts_are_no_commands() {
        for text in ["hello", "ask", "pick", "sending", "", "  "] {
            assert!(parse_command(text).is_none(), "{text}");
        }
    }

    #[test]
    fn targets_and_actions_round_trip_through_json() {
        let group = ConversationId::new();
        let action = Action::SendMessage {
            to: Target::Group(group),
            text: "hi".into(),
        };
        let json = serde_json::to_value(&action).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"action": "send_message", "to": format!("group:{group}"), "text": "hi"})
        );
        assert_eq!(serde_json::from_value::<Action>(json).unwrap(), action);
        let call: Action =
            serde_json::from_value(serde_json::json!({"action": "call", "to": "bob"})).unwrap();
        assert_eq!(
            call,
            Action::Call {
                to: Target::Handle("bob".into()),
                reason: None
            }
        );
        assert!(
            serde_json::from_value::<Action>(
                serde_json::json!({"action": "send_message", "to": "", "text": "x"})
            )
            .is_err()
        );
        let result = ActionResult::done("sent").with_conversation("c");
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({"code": "done", "detail": "sent", "conversation": "c"})
        );
    }

    fn server(code: &str) -> SdkError {
        server_with(code, None, None)
    }

    fn server_with(code: &str, retry_after_s: Option<u32>, action_id: Option<&str>) -> SdkError {
        SdkError::Server {
            code: code.into(),
            message: "why".into(),
            retryable: false,
            retry_after_s,
            action_id: action_id.map(str::to_owned),
        }
    }

    #[test]
    fn server_errors_map_to_stable_codes() {
        let waiting = server_with("waiting_for_owner", None, Some("a-1"));
        let result = ActionResult::from_sdk(&waiting);
        assert_eq!(
            (result.code, result.action_id.as_deref()),
            (ActionCode::WaitingForOwner, Some("a-1"))
        );
        let limited = server_with("rate_limited", Some(42), None);
        let result = ActionResult::from_sdk(&limited);
        assert_eq!(
            (result.code, result.retry_after_s),
            (ActionCode::RateLimited, Some(42))
        );
        for (code, expected) in [
            ("not_allowed_by_owner", ActionCode::NotAllowedByOwner),
            ("not_contact", ActionCode::NotContact),
            ("user_not_found", ActionCode::NotFound),
            ("not_found", ActionCode::NotFound),
            ("not_allowed", ActionCode::NotAllowed),
            ("not_member", ActionCode::NotAllowed),
            ("not_available", ActionCode::Unavailable),
            ("group_full", ActionCode::Failed),
        ] {
            assert_eq!(
                ActionResult::from_sdk(&server(code)).code,
                expected,
                "{code}"
            );
        }
        assert_eq!(
            ActionResult::from_sdk(&SdkError::Offline).code,
            ActionCode::Unavailable
        );
        assert_eq!(
            ActionResult::from_sdk(&SdkError::Timeout).code,
            ActionCode::Failed
        );
    }

    #[test]
    fn summaries_read_as_the_spec_says() {
        let mut waiting = ActionResult::new(ActionCode::WaitingForOwner, "x");
        waiting.action_id = Some("a-1".into());
        assert_eq!(waiting.summary(), "waiting for owner (action a-1)");
        let mut limited = ActionResult::new(ActionCode::RateLimited, "x");
        limited.retry_after_s = Some(30);
        assert_eq!(limited.summary(), "rate limited, retry in 30 s");
        assert_eq!(ActionResult::done("sent").summary(), "done: sent");
        assert_eq!(
            ActionResult::new(ActionCode::NotAllowedByOwner, "x").summary(),
            "not allowed by owner"
        );
        assert_eq!(
            ActionResult::new(ActionCode::NotFound, "no user @x").summary(),
            "not found: no user @x"
        );
    }
}
