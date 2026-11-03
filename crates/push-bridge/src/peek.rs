//! The chat peek: the owner conversation's last messages, kept in memory only (never
//! written anywhere, gone when the bridge exits) for the tray's `peek` over the control
//! channel. Every entry is masked as it was sent (the owner's texts are masked here) and
//! cut to [`PEEK_TEXT_MAX`] characters.

use std::collections::VecDeque;

use push_bridge_control::{CardState, PEEK_CAP, PEEK_TEXT_MAX, PeekEntry, PeekFrom, PeekKind};
use push_sdk::ClientMsgId;
use serde_json::Value;

use crate::mask::{cap, masked};

/// One entry derived from a RICH body the bridge sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub kind: PeekKind,
    pub text: String,
    pub card_state: Option<CardState>,
    /// A card's choices: `(value, label)`.
    pub choices: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    entry: PeekEntry,
    /// The message the entry came from (`None` for the owner's).
    id: Option<ClientMsgId>,
    /// The kind of the message's text entry (reply or note), kept across edits.
    text_kind: PeekKind,
    choices: Vec<(String, String)>,
    /// Holds the place of a message with nothing to show yet (a reply with no text
    /// yet), so its first edit lands where it was sent; never returned.
    hidden: bool,
}

/// The last [`PEEK_CAP`] entries of the owner conversation, oldest first.
#[derive(Debug, Default)]
pub struct PeekRing {
    entries: VecDeque<Stored>,
    /// Counts the changes, so callers can tell whether anything changed.
    revision: u64,
}

impl PeekRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Goes up with every change of the entries returned.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The owner sent `text` (masked here).
    pub fn owner_text(&mut self, time: i64, text: &str) {
        self.push(owner(time, PeekKind::Text, masked(text, PEEK_TEXT_MAX)));
    }

    /// The owner answered `card` with `values`: the chosen choices' labels (unknown
    /// values masked, as they are), and the card is answered.
    pub fn owner_answer(&mut self, time: i64, card: ClientMsgId, values: &[String]) {
        let choices = self
            .entries
            .iter()
            .find(|stored| stored.id == Some(card) && stored.entry.kind == PeekKind::Card)
            .map(|stored| stored.choices.clone())
            .unwrap_or_default();
        let labels: Vec<String> = values
            .iter()
            .map(|value| {
                choices
                    .iter()
                    .find(|(id, _)| id == value)
                    .map_or_else(|| masked(value, PEEK_TEXT_MAX), |(_, label)| label.clone())
            })
            .collect();
        self.card_state(card, CardState::Answered);
        self.push(owner(
            time,
            PeekKind::Answer,
            cap(&labels.join(", "), PEEK_TEXT_MAX),
        ));
    }

    /// The bridge sent the RICH `body` as message `id`; `text_kind` is the kind of its
    /// text ([`PeekKind::Reply`] for the agent's, [`PeekKind::Note`] for the bridge's own
    /// words).
    pub fn sent(&mut self, time: i64, id: Option<ClientMsgId>, body: &str, text_kind: PeekKind) {
        for stored in message(time, id, body, text_kind) {
            self.push(stored);
        }
    }

    /// Message `id` was edited to `body`: its entries are derived again and replace the
    /// old ones where they were (with their time, text kind and card state); when none
    /// are left in the ring, they are added as new replies.
    pub fn edited(&mut self, time: i64, id: ClientMsgId, body: &str) {
        let Some(first) = self.entries.iter().position(|stored| stored.id == Some(id)) else {
            return self.sent(time, Some(id), body, PeekKind::Reply);
        };
        let old = &self.entries[first];
        let (time, text_kind) = (old.entry.time, old.text_kind);
        let state = self
            .entries
            .iter()
            .find(|stored| stored.id == Some(id) && stored.entry.card_state.is_some())
            .and_then(|stored| stored.entry.card_state);
        let mut new = message(time, Some(id), body, text_kind);
        for stored in &mut new {
            if stored.entry.card_state.is_some() && state.is_some() {
                stored.entry.card_state = state;
            }
        }
        let old: Vec<&Stored> = self
            .entries
            .iter()
            .filter(|stored| stored.id == Some(id))
            .collect();
        if old.len() == new.len() && old.iter().zip(&new).all(|(a, b)| **a == *b) {
            return;
        }
        let mut before = std::mem::take(&mut self.entries);
        let after = before.split_off(first);
        self.entries = before;
        self.entries.extend(new);
        self.entries
            .extend(after.into_iter().filter(|stored| stored.id != Some(id)));
        self.trim();
        self.revision += 1;
    }

    /// Sets the state of `card`'s entry; `true` when it changed.
    pub fn card_state(&mut self, card: ClientMsgId, state: CardState) -> bool {
        let mut changed = false;
        for stored in &mut self.entries {
            if stored.id == Some(card)
                && stored.entry.kind == PeekKind::Card
                && stored.entry.card_state != Some(state)
            {
                stored.entry.card_state = Some(state);
                changed = true;
            }
        }
        if changed {
            self.revision += 1;
        }
        changed
    }

    /// The last `limit` entries, oldest first.
    pub fn last(&self, limit: usize) -> Vec<PeekEntry> {
        let shown: Vec<&Stored> = self.entries.iter().filter(|s| !s.hidden).collect();
        let skip = shown.len().saturating_sub(limit);
        shown[skip..].iter().map(|s| s.entry.clone()).collect()
    }

    fn push(&mut self, stored: Stored) {
        let hidden = stored.hidden;
        self.entries.push_back(stored);
        self.trim();
        if !hidden {
            self.revision += 1;
        }
    }

    /// At most [`PEEK_CAP`] entries shown and as many placeholders.
    fn trim(&mut self) {
        while self.entries.len() > 2 * PEEK_CAP
            || self.entries.iter().filter(|s| !s.hidden).count() > PEEK_CAP
        {
            self.entries.pop_front();
        }
    }
}

fn owner(time: i64, kind: PeekKind, text: String) -> Stored {
    Stored {
        entry: PeekEntry {
            time,
            from: PeekFrom::Owner,
            kind,
            text,
            card_state: None,
        },
        id: None,
        text_kind: kind,
        choices: Vec::new(),
        hidden: false,
    }
}

/// A sent message's entries: derived from `body`, or a placeholder when there is
/// nothing to show yet.
fn message(time: i64, id: Option<ClientMsgId>, body: &str, text_kind: PeekKind) -> Vec<Stored> {
    let agent = |kind: PeekKind, text: String, card_state, choices, hidden| Stored {
        entry: PeekEntry {
            time,
            from: PeekFrom::Agent,
            kind,
            text,
            card_state,
        },
        id,
        text_kind,
        choices,
        hidden,
    };
    let derived = derive(body, text_kind);
    if derived.is_empty() {
        return match id {
            Some(_) => vec![agent(text_kind, String::new(), None, Vec::new(), true)],
            None => Vec::new(),
        };
    }
    derived
        .into_iter()
        .map(|d| agent(d.kind, d.text, d.card_state, d.choices, false))
        .collect()
}

/// The entries of a RICH body (`{"tpl", "params", "fallback"}`) as sent: a reply's steps
/// and its text (not its thinking), a card with its choices, or else the fallback.
pub fn derive(body: &str, text_kind: PeekKind) -> Vec<Derived> {
    let Ok(body) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let params = &body["params"];
    let string = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let entry = |kind: PeekKind, text: String| Derived {
        kind,
        text: cap(&text, PEEK_TEXT_MAX),
        card_state: None,
        choices: Vec::new(),
    };
    let card = |text: String, choices: Vec<(String, String)>| {
        let mut text = text;
        for (_, label) in &choices {
            text.push_str(&format!(" [{label}]"));
        }
        Derived {
            kind: PeekKind::Card,
            text: cap(&text, PEEK_TEXT_MAX),
            card_state: Some(CardState::Open),
            choices,
        }
    };
    let approval = || {
        vec![
            ("approve".to_owned(), "Approve".to_owned()),
            ("reject".to_owned(), "Reject".to_owned()),
        ]
    };
    match body["tpl"].as_str().unwrap_or_default() {
        "agent.reply@1" | "agent.reply@2" => {
            let mut entries: Vec<Derived> = params["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|step| {
                    let mark = match step["status"].as_str() {
                        Some("done") => "✓",
                        Some("failed") => "✗",
                        _ => "…",
                    };
                    entry(PeekKind::Step, format!("{mark} {}", string(&step["label"])))
                })
                .collect();
            let text = string(&params["text"]);
            if !text.is_empty() && text != "…" {
                let text = match params["title"].as_str() {
                    Some(title) => format!("{title}: {text}"),
                    None => text,
                };
                entries.push(entry(text_kind, text));
            }
            entries
        }
        "agent.approval@1" | "agent.plan-approval@1" => {
            vec![card(string(&params["title"]), approval())]
        }
        "agent.question@1" | "agent.choose@1" => {
            let choices = params["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|option| (string(&option["id"]), string(&option["label"])))
                .collect();
            vec![card(string(&params["question"]), choices)]
        }
        _ => vec![entry(text_kind, string(&body["fallback"]))],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mask::MASK;

    fn reply(title: Option<&str>, text: &str, steps: Value) -> String {
        let mut params = json!({"text": text, "thinking": "secret thoughts", "steps": steps});
        if let Some(title) = title {
            params["title"] = json!(title);
        }
        json!({"tpl": "agent.reply@1", "params": params, "fallback": text}).to_string()
    }

    fn texts(ring: &PeekRing) -> Vec<String> {
        ring.last(PEEK_CAP).into_iter().map(|e| e.text).collect()
    }

    fn approval_card() -> String {
        json!({
            "tpl": "agent.approval@1",
            "params": {"title": "Allow Bash?", "details": "d"},
            "fallback": "Allow Bash?"
        })
        .to_string()
    }

    #[test]
    fn the_last_entries_are_kept_oldest_first() {
        let mut ring = PeekRing::new();
        for i in 0..120 {
            ring.owner_text(i, &format!("m{i}"));
        }
        assert_eq!(ring.last(PEEK_CAP).len(), PEEK_CAP);
        let last = ring.last(20);
        let expected: Vec<String> = (100..120).map(|i| format!("m{i}")).collect();
        assert_eq!(
            last.iter().map(|e| e.text.clone()).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(last[0].time, 100);
    }

    #[test]
    fn fewer_entries_than_the_limit_are_all_returned() {
        let mut ring = PeekRing::new();
        assert!(ring.last(20).is_empty());
        ring.owner_text(1, "a");
        ring.owner_text(2, "b");
        assert_eq!(texts(&ring), ["a", "b"]);
        assert_eq!(ring.last(1)[0].text, "b");
    }

    #[test]
    fn the_owner_s_text_is_masked_and_capped() {
        let mut ring = PeekRing::new();
        ring.owner_text(1, "use api_key=hunter2 ok");
        let entry = &ring.last(1)[0];
        assert_eq!(
            (entry.from, entry.kind, entry.card_state),
            (PeekFrom::Owner, PeekKind::Text, None)
        );
        assert!(entry.text.contains(MASK), "{}", entry.text);
        assert!(!entry.text.contains("hunter2"));
        ring.owner_text(2, &"x".repeat(1_000));
        let long = &ring.last(1)[0].text;
        assert_eq!(long.chars().count(), PEEK_TEXT_MAX);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_reply_gives_its_steps_then_its_text_without_thinking() {
        let steps = json!([
            {"label": "run_command dotnet test", "status": "done"},
            {"label": "edit a.rs", "status": "failed", "detail": "no"},
            {"label": "read b.rs", "status": "running"},
            {"label": "grep x", "status": "pending"},
        ]);
        let derived = derive(&reply(Some("Fix"), "All green.", steps), PeekKind::Reply);
        let shown: Vec<_> = derived.iter().map(|d| (d.kind, d.text.as_str())).collect();
        assert_eq!(
            shown,
            [
                (PeekKind::Step, "✓ run_command dotnet test"),
                (PeekKind::Step, "✗ edit a.rs"),
                (PeekKind::Step, "… read b.rs"),
                (PeekKind::Step, "… grep x"),
                (PeekKind::Reply, "Fix: All green."),
            ]
        );
        assert!(derived.iter().all(|d| d.card_state.is_none()));
        let placeholder = derive(&reply(None, "…", json!([])), PeekKind::Note);
        assert!(placeholder.is_empty());
        let note = derive(&reply(None, "Hi", Value::Null), PeekKind::Note);
        assert_eq!(
            (note[0].kind, note[0].text.as_str()),
            (PeekKind::Note, "Hi")
        );
    }

    #[test]
    fn a_long_text_is_cut_after_its_title() {
        let derived = derive(
            &reply(Some("T"), &"y".repeat(500), json!([])),
            PeekKind::Reply,
        );
        assert_eq!(derived[0].text.chars().count(), PEEK_TEXT_MAX);
        assert!(derived[0].text.starts_with("T: yyy"));
        assert!(derived[0].text.ends_with('…'));
    }

    #[test]
    fn cards_list_their_choices_and_answers_show_labels() {
        let question = json!({
            "tpl": "agent.choose@1",
            "params": {"question": "Which?", "options": [
                {"id": "o1", "label": "Red"}, {"id": "o2", "label": "Blue"}
            ]},
            "fallback": "Which?"
        })
        .to_string();
        let plan = json!({
            "tpl": "agent.plan-approval@1",
            "params": {"title": "Approve the plan?", "plan": "1. x"},
            "fallback": "Approve the plan?"
        })
        .to_string();
        let (card, other, plan_id) = (ClientMsgId::new(), ClientMsgId::new(), ClientMsgId::new());
        let mut ring = PeekRing::new();
        ring.sent(1, Some(card), &question, PeekKind::Reply);
        ring.sent(2, Some(other), &approval_card(), PeekKind::Reply);
        ring.sent(3, Some(plan_id), &plan, PeekKind::Reply);
        let entries = ring.last(3);
        assert_eq!(entries[0].text, "Which? [Red] [Blue]");
        assert_eq!(entries[1].text, "Allow Bash? [Approve] [Reject]");
        assert_eq!(entries[2].text, "Approve the plan? [Approve] [Reject]");
        assert!(
            entries
                .iter()
                .all(|e| e.kind == PeekKind::Card && e.card_state == Some(CardState::Open))
        );

        ring.owner_answer(4, card, &["o2".into(), "o1".into(), "api_key=x1".into()]);
        let entries = ring.last(PEEK_CAP);
        let answer = entries.last().unwrap();
        assert_eq!(
            (answer.from, answer.kind),
            (PeekFrom::Owner, PeekKind::Answer)
        );
        assert_eq!(answer.text, format!("Blue, Red, api_key={MASK}"));
        assert_eq!(entries[0].card_state, Some(CardState::Answered));
        assert_eq!(entries[1].card_state, Some(CardState::Open));

        ring.owner_answer(5, other, &["approve".into()]);
        assert_eq!(ring.last(1)[0].text, "Approve");
        assert!(ring.card_state(plan_id, CardState::Expired));
        assert!(!ring.card_state(plan_id, CardState::Expired));
        assert!(ring.card_state(other, CardState::Declined));
        let states: Vec<_> = ring.last(PEEK_CAP).iter().map(|e| e.card_state).collect();
        assert_eq!(
            states,
            [
                Some(CardState::Answered),
                Some(CardState::Declined),
                Some(CardState::Expired),
                None,
                None
            ]
        );
    }

    #[test]
    fn an_edit_replaces_the_message_s_entries_in_place() {
        let mut ring = PeekRing::new();
        let id = ClientMsgId::new();
        ring.owner_text(1, "go");
        ring.sent(2, Some(id), &reply(None, "…", json!([])), PeekKind::Note);
        ring.owner_text(3, "and then");
        let mut text = String::new();
        for i in 0..5 {
            text.push_str(&format!("w{i} "));
            let steps =
                json!([{"label": "bash ls", "status": if i < 4 { "running" } else { "done" }}]);
            ring.edited(10 + i, id, &reply(None, text.trim(), steps));
        }
        let entries = ring.last(PEEK_CAP);
        let shown: Vec<_> = entries.iter().map(|e| (e.kind, e.text.as_str())).collect();
        assert_eq!(
            shown,
            [
                (PeekKind::Text, "go"),
                (PeekKind::Step, "✓ bash ls"),
                (PeekKind::Note, "w0 w1 w2 w3 w4"),
                (PeekKind::Text, "and then"),
            ]
        );
        assert_eq!(entries[2].time, 2, "the message's time");
        let revision = ring.revision();
        ring.edited(
            20,
            id,
            &reply(
                None,
                "w0 w1 w2 w3 w4",
                json!([{"label": "bash ls", "status": "done"}]),
            ),
        );
        assert_eq!(ring.revision(), revision, "an edit that changes nothing");
    }

    #[test]
    fn an_edit_keeps_the_card_state_and_an_unknown_message_is_added() {
        let mut ring = PeekRing::new();
        let card = ClientMsgId::new();
        ring.sent(1, Some(card), &approval_card(), PeekKind::Reply);
        ring.card_state(card, CardState::Expired);
        ring.edited(2, card, &approval_card());
        assert_eq!(ring.last(1)[0].card_state, Some(CardState::Expired));
        let gone = ClientMsgId::new();
        ring.edited(3, gone, &reply(None, "late", json!([])));
        let last = &ring.last(1)[0];
        assert_eq!(
            (last.kind, last.text.as_str(), last.time),
            (PeekKind::Reply, "late", 3)
        );
    }

    #[test]
    fn an_unknown_template_shows_its_fallback() {
        let body =
            json!({"tpl": "agent.other@1", "params": {}, "fallback": "plain words"}).to_string();
        let derived = derive(&body, PeekKind::Reply);
        assert_eq!(derived.len(), 1);
        assert_eq!(
            (
                derived[0].kind,
                derived[0].text.as_str(),
                derived[0].card_state
            ),
            (PeekKind::Reply, "plain words", None)
        );
    }
}
