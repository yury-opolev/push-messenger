//! The agent's requests as cards, and the owner's answers back as backend answers.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, json};

use crate::backend::Answer;
use crate::mask::{cap, mask};
use crate::render::{clean, pages, reply_body, rich_body, shown, shown_line};

/// How long a card waits for its answer.
pub const CARD_TIMEOUT: Duration = Duration::from_secs(600);

/// Characters of a card's title.
const TITLE: usize = 120;
/// Characters of approval details, previews and plans.
const LONG_TEXT: usize = 4_000;
/// Characters of a question.
const QUESTION: usize = 1_000;
/// Characters of an option label (push-domain's limit for options in a body).
const OPTION_LABEL: usize = 80;
/// Options a card offers; more are shown as text.
const MAX_OPTIONS: usize = 10;
const APPROVE: &str = "approve";
const REJECT: &str = "reject";
const PLAN_CUT: &str = "\n\n(The plan is cut here; the full text follows.)";
const PICK_ONE: &str = "Pick one of the options";
const TYPE_ANSWER: &str = "Type an answer";

/// What an open card asks.
#[derive(Clone, PartialEq, Eq)]
pub enum CardKind {
    Permission,
    /// `options` are the agent's labels, unmasked: answers carry them back.
    Question {
        options: Vec<String>,
        multi: bool,
        free_text: bool,
    },
    Plan,
    /// Turn off approvals (`/mode bypass`).
    Bypass,
}

impl fmt::Debug for CardKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Permission => formatter.write_str("Permission"),
            Self::Question {
                options,
                multi,
                free_text,
            } => formatter
                .debug_struct("Question")
                .field("options", &options.len())
                .field("multi", multi)
                .field("free_text", free_text)
                .finish(),
            Self::Plan => formatter.write_str("Plan"),
            Self::Bypass => formatter.write_str("Bypass"),
        }
    }
}

/// A request ready to post.
pub struct CardPlan {
    /// RICH body of the card, or `None` when the request is shown as text only (more than
    /// 10 options).
    pub card: Option<String>,
    /// `agent.reply@1` bodies to post after the card (the full plan, the text-only question).
    pub follow_ups: Vec<String>,
    /// The values the card offers, for the CardBook.
    pub offered: Vec<String>,
    /// Most values one answer may hold.
    pub max_values: usize,
    pub kind: CardKind,
    /// The activity label while the card is open.
    pub waiting_label: &'static str,
}

/// `agent.approval@1` asking to use `tool` in `folder`, with the masked preview.
pub fn permission_card(backend: &str, tool: &str, preview: &str, folder: &Path) -> CardPlan {
    let title = shown_line(&format!("Allow {tool}?"), TITLE);
    let details = format!(
        "{backend} wants to use {tool} in {}.",
        folder.display(),
        tool = shown_line(tool, TITLE),
    );
    approval(
        title,
        &details,
        Some(preview),
        CardKind::Permission,
        "approval needed",
    )
}

/// `agent.approval@1` asking to turn approvals off until `/new`.
pub fn bypass_card(backend: &str) -> CardPlan {
    let details = format!(
        "{backend} will run every tool without asking until /new. Approve to switch to bypassPermissions."
    );
    approval(
        "Turn off approvals?".to_owned(),
        &details,
        None,
        CardKind::Bypass,
        "approval needed",
    )
}

fn approval(
    title: String,
    details: &str,
    preview: Option<&str>,
    kind: CardKind,
    waiting_label: &'static str,
) -> CardPlan {
    let mut params = Map::new();
    params.insert("title".into(), json!(title));
    params.insert(
        "details".into(),
        json!(non_empty(shown(details, LONG_TEXT))),
    );
    let preview = preview.map(|preview| shown(preview, LONG_TEXT));
    if let Some(preview) = preview.filter(|preview| !preview.trim().is_empty()) {
        params.insert("preview".into(), json!(preview));
    }
    CardPlan {
        card: Some(rich_body("agent.approval@1", params, &title)),
        follow_ups: Vec::new(),
        offered: vec![APPROVE.to_owned(), REJECT.to_owned()],
        max_values: 1,
        kind,
        waiting_label,
    }
}

/// `agent.question@1` (single choice) or `agent.choose@1` (multi-select) with option ids
/// `o1`..`o10`; with more than 10 options (or none), a text-only `agent.reply@1` instead.
pub fn question_card(question: &str, options: &[String], multi: bool, free_text: bool) -> CardPlan {
    let kind = CardKind::Question {
        options: options.to_vec(),
        multi,
        free_text,
    };
    if options.is_empty() || options.len() > MAX_OPTIONS {
        return CardPlan {
            card: None,
            follow_ups: text_question(question, options, multi, free_text),
            offered: Vec::new(),
            max_values: 0,
            kind,
            waiting_label: "question",
        };
    }
    let shown_question = non_empty(shown(question, QUESTION));
    let items: Vec<_> = options
        .iter()
        .enumerate()
        .map(|(i, label)| json!({"id": option_id(i), "label": non_empty(shown_line(label, OPTION_LABEL))}))
        .collect();
    let mut params = Map::new();
    params.insert("question".into(), json!(shown_question));
    params.insert("options".into(), json!(items));
    let tpl = if multi {
        "agent.choose@1"
    } else {
        "agent.question@1"
    };
    CardPlan {
        card: Some(rich_body(tpl, params, &cap(&shown_question, 2_000))),
        follow_ups: Vec::new(),
        offered: (0..options.len()).map(option_id).collect(),
        max_values: if multi { options.len() } else { 1 },
        kind,
        waiting_label: "question",
    }
}

fn text_question(question: &str, options: &[String], multi: bool, free_text: bool) -> Vec<String> {
    let mut text = clean(question, true);
    if !options.is_empty() {
        text.push_str("\n\n");
        for (i, label) in options.iter().enumerate() {
            text.push_str(&format!("{}. {}\n", i + 1, clean(label, false)));
        }
    }
    text.push('\n');
    text.push_str(match (options.is_empty(), multi) {
        (true, _) => "Reply with your answer.",
        (false, false) => "Reply with the option's number or label.",
        (false, true) => "Reply with the options' numbers or labels, separated by commas.",
    });
    if free_text && !options.is_empty() {
        text.push_str(" Or type your own answer.");
    }
    pages(&mask(&text))
        .iter()
        .map(|page| reply_body(Some("Question"), page, None, &[]))
        .collect()
}

/// `agent.plan-approval@1` with the masked plan; a plan over 4,000 characters is cut with a
/// note, and the full plan follows as `agent.reply@1` pages titled "Plan (1/N)".
pub fn plan_card(plan: &str) -> CardPlan {
    let full = mask(&clean(plan, true));
    let chars: Vec<char> = full.chars().collect();
    let (shown_plan, follow_ups) = if chars.len() > LONG_TEXT {
        let keep = LONG_TEXT - PLAN_CUT.chars().count();
        let mut cut: String = chars[..keep].iter().collect();
        cut.push_str(PLAN_CUT);
        let pages = pages(&full);
        let count = pages.len();
        let follow_ups = pages
            .iter()
            .enumerate()
            .map(|(i, page)| {
                let title = format!("Plan ({}/{count})", i + 1);
                reply_body(Some(&title), page, None, &[])
            })
            .collect();
        (cut, follow_ups)
    } else {
        (non_empty(full), Vec::new())
    };
    let mut params = Map::new();
    params.insert("title".into(), json!("Approve the plan?"));
    params.insert("plan".into(), json!(shown_plan));
    CardPlan {
        card: Some(rich_body(
            "agent.plan-approval@1",
            params,
            "Approve the plan?",
        )),
        follow_ups,
        offered: vec![APPROVE.to_owned(), REJECT.to_owned()],
        max_values: 1,
        kind: CardKind::Plan,
        waiting_label: "plan approval",
    }
}

/// From the owner's card answer values to the backend answer.
pub fn answer_from_values(kind: &CardKind, values: &[String]) -> Answer {
    let approved = values.len() == 1 && values[0] == APPROVE;
    match kind {
        CardKind::Permission | CardKind::Bypass => Answer::Permission(approved),
        CardKind::Plan => Answer::Plan(approved),
        CardKind::Question { options, .. } => {
            let chosen: Vec<usize> = values
                .iter()
                .filter_map(|value| (0..options.len()).find(|&i| option_id(i) == *value))
                .collect();
            labels(options, chosen)
        }
    }
}

/// What typed text does to the oldest open card.
#[derive(Clone, PartialEq, Eq)]
pub enum TypedOutcome {
    Answer(Answer),
    /// The text does not answer the card: reply this, and the card stays open.
    Retry(&'static str),
}

impl fmt::Debug for TypedOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Answer(answer) => write!(formatter, "Answer({answer:?})"),
            Self::Retry(reply) => write!(formatter, "Retry({reply:?})"),
        }
    }
}

/// The owner typed text while this card is the oldest open one.
pub fn answer_from_text(kind: &CardKind, text: &str) -> TypedOutcome {
    let text = text.trim();
    let yes = text.eq_ignore_ascii_case("yes") || text.eq_ignore_ascii_case("y");
    match kind {
        CardKind::Permission | CardKind::Bypass => TypedOutcome::Answer(Answer::Permission(yes)),
        CardKind::Plan => TypedOutcome::Answer(Answer::Plan(false)),
        CardKind::Question {
            options,
            multi,
            free_text,
        } => {
            let retry = if *free_text || options.is_empty() {
                TYPE_ANSWER
            } else {
                PICK_ONE
            };
            if text.is_empty() {
                return TypedOutcome::Retry(retry);
            }
            let chosen = match pick(options, text) {
                Some(i) => Some(vec![i]),
                None if *multi => text
                    .split(',')
                    .map(|part| pick(options, part.trim()))
                    .collect::<Option<Vec<usize>>>(),
                None => None,
            };
            match chosen {
                Some(chosen) => TypedOutcome::Answer(labels(options, chosen)),
                None if *free_text || options.is_empty() => {
                    TypedOutcome::Answer(Answer::Question(text.to_owned()))
                }
                None => TypedOutcome::Retry(retry),
            }
        }
    }
}

/// What the backend gets when the card times out; `None`: nothing (bypass stays off).
pub fn timeout_answer(kind: &CardKind) -> Option<Answer> {
    match kind {
        CardKind::Permission => Some(Answer::Permission(false)),
        CardKind::Plan => Some(Answer::Plan(false)),
        CardKind::Question { .. } => Some(Answer::Decline),
        CardKind::Bypass => None,
    }
}

fn option_id(index: usize) -> String {
    format!("o{}", index + 1)
}

/// The option whose label is `text` (case-insensitive), else whose 1-based number it is.
fn pick(options: &[String], text: &str) -> Option<usize> {
    let lower = text.to_lowercase();
    options
        .iter()
        .position(|label| label.trim().to_lowercase() == lower)
        .or_else(|| {
            text.parse::<usize>()
                .ok()
                .filter(|n| (1..=options.len()).contains(n))
                .map(|n| n - 1)
        })
}

/// The chosen labels in option order, joined with ", "; declined when nothing is left.
fn labels(options: &[String], mut chosen: Vec<usize>) -> Answer {
    chosen.sort_unstable();
    chosen.dedup();
    let joined = chosen
        .iter()
        .map(|&i| options[i].as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if joined.trim().is_empty() {
        Answer::Decline
    } else {
        Answer::Question(joined)
    }
}

fn non_empty(text: String) -> String {
    if text.trim().is_empty() {
        "…".to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use push_domain::templates::{ParamValue, RichBody, builtins};

    use super::*;
    use crate::render::tests::{param, valid};

    fn offered(body: &str, tpl: &str) -> Vec<String> {
        let definition = builtins()
            .into_iter()
            .find(|d| d.template_ref().to_string() == tpl)
            .unwrap();
        RichBody::parse(body.as_bytes())
            .unwrap()
            .offered(&definition)
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn question(answer: &str) -> Answer {
        Answer::Question(answer.to_owned())
    }

    #[test]
    fn a_permission_card_is_a_valid_approval_with_a_masked_preview() {
        let plan = permission_card(
            "Coda",
            "bash",
            "export API_KEY=abc123\r\nmake\tdeploy",
            Path::new("/work/app"),
        );
        let card = plan.card.as_deref().unwrap();
        let body = valid(card, "agent.approval@1");
        assert_eq!(param(&body, "title").as_deref(), Some("Allow bash?"));
        assert_eq!(
            param(&body, "details").as_deref(),
            Some("Coda wants to use bash in /work/app.")
        );
        assert_eq!(
            param(&body, "preview").as_deref(),
            Some("export API_KEY=«masked»\nmake    deploy")
        );
        assert_eq!(plan.offered, offered(card, "agent.approval@1"));
        assert_eq!(
            (plan.max_values, plan.waiting_label),
            (1, "approval needed")
        );
        assert_eq!(plan.kind, CardKind::Permission);
        assert!(plan.follow_ups.is_empty());
    }

    #[test]
    fn an_empty_preview_is_left_out_and_a_long_one_capped() {
        let plan = permission_card("Coda", "edit", " ", Path::new("/w"));
        let body = valid(plan.card.as_deref().unwrap(), "agent.approval@1");
        assert_eq!(body.params.get("preview"), None);
        let long = "line\n".repeat(2_000);
        let plan = permission_card("Coda", "edit", &long, Path::new("/w"));
        let body = valid(plan.card.as_deref().unwrap(), "agent.approval@1");
        assert_eq!(param(&body, "preview").unwrap().chars().count(), LONG_TEXT);
    }

    #[test]
    fn a_bypass_card_is_a_valid_approval() {
        let plan = bypass_card("Coda");
        let card = plan.card.as_deref().unwrap();
        let body = valid(card, "agent.approval@1");
        assert_eq!(
            param(&body, "title").as_deref(),
            Some("Turn off approvals?")
        );
        assert!(param(&body, "details").unwrap().contains("until /new"));
        assert_eq!(plan.offered, offered(card, "agent.approval@1"));
        assert_eq!(plan.kind, CardKind::Bypass);
    }

    #[test]
    fn a_single_choice_question_is_an_agent_question() {
        let plan = question_card("Which region?", &strings(&["EU", "US"]), false, false);
        let card = plan.card.as_deref().unwrap();
        let body = valid(card, "agent.question@1");
        assert_eq!(param(&body, "question").as_deref(), Some("Which region?"));
        assert_eq!(plan.offered, ["o1", "o2"]);
        assert_eq!(plan.offered, offered(card, "agent.question@1"));
        assert_eq!((plan.max_values, plan.waiting_label), (1, "question"));
    }

    #[test]
    fn a_multi_select_question_is_an_agent_choose() {
        let options: Vec<String> = (1..=10).map(|i| format!("Check {i}")).collect();
        let plan = question_card("Which checks?", &options, true, false);
        let card = plan.card.as_deref().unwrap();
        valid(card, "agent.choose@1");
        assert_eq!(plan.offered, offered(card, "agent.choose@1"));
        assert_eq!(plan.offered.last().map(String::as_str), Some("o10"));
        assert_eq!(plan.max_values, 10);
    }

    #[test]
    fn option_labels_are_masked_and_capped() {
        let options = strings(&["token=s3cr3t", &"long ".repeat(40), ""]);
        let plan = question_card("Pick", &options, false, false);
        let body = valid(plan.card.as_deref().unwrap(), "agent.question@1");
        let Some(ParamValue::List(items)) = body.params.get("options") else {
            panic!("options");
        };
        let labels: Vec<String> = items
            .iter()
            .map(|item| match item {
                push_domain::templates::ListItem::Option { label, .. } => label.clone(),
                _ => panic!("an option"),
            })
            .collect();
        assert_eq!(labels[0], "token=«masked»");
        assert_eq!(labels[1].chars().count(), OPTION_LABEL);
        assert_eq!(labels[2], "…");
    }

    #[test]
    fn eleven_options_are_shown_as_text() {
        let options: Vec<String> = (1..=11).map(|i| format!("Option {i}")).collect();
        let plan = question_card("Which one?", &options, false, false);
        assert!(plan.card.is_none());
        assert!(plan.offered.is_empty());
        assert_eq!(plan.follow_ups.len(), 1);
        let body = valid(&plan.follow_ups[0], "agent.reply@1");
        let text = param(&body, "text").unwrap();
        assert!(text.starts_with("Which one?\n\n1. Option 1\n"));
        assert!(text.contains("11. Option 11\n"));
        assert!(text.ends_with("Reply with the option's number or label."));
        let multi = question_card("Which?", &options, true, false);
        let body = valid(&multi.follow_ups[0], "agent.reply@1");
        assert!(
            param(&body, "text")
                .unwrap()
                .ends_with("separated by commas.")
        );
    }

    #[test]
    fn a_short_plan_is_shown_whole() {
        let plan = plan_card("1. Refactor\n2. Test");
        let card = plan.card.as_deref().unwrap();
        let body = valid(card, "agent.plan-approval@1");
        assert_eq!(
            param(&body, "plan").as_deref(),
            Some("1. Refactor\n2. Test")
        );
        assert_eq!(param(&body, "title").as_deref(), Some("Approve the plan?"));
        assert_eq!(plan.offered, offered(card, "agent.plan-approval@1"));
        assert!(plan.follow_ups.is_empty());
        assert_eq!(plan.waiting_label, "plan approval");
    }

    #[test]
    fn a_long_plan_is_cut_with_a_note_and_follows_in_full() {
        let text = format!("secret=hunter2\n{}", "step\n".repeat(1_700));
        let plan = plan_card(&text);
        let body = valid(plan.card.as_deref().unwrap(), "agent.plan-approval@1");
        let shown = param(&body, "plan").unwrap();
        assert_eq!(shown.chars().count(), LONG_TEXT);
        assert!(shown.ends_with(PLAN_CUT));
        assert!(!shown.contains("hunter2"));
        assert_eq!(plan.follow_ups.len(), 3);
        let mut full = String::new();
        for (i, follow_up) in plan.follow_ups.iter().enumerate() {
            let page = valid(follow_up, "agent.reply@1");
            assert_eq!(param(&page, "title"), Some(format!("Plan ({}/3)", i + 1)));
            full.push_str(&param(&page, "text").unwrap());
        }
        assert_eq!(full, format!("secret=«masked»\n{}", "step\n".repeat(1_700)));
    }

    #[test]
    fn card_values_become_answers() {
        let approve = strings(&["approve"]);
        let reject = strings(&["reject"]);
        assert_eq!(
            answer_from_values(&CardKind::Permission, &approve),
            Answer::Permission(true)
        );
        assert_eq!(
            answer_from_values(&CardKind::Permission, &reject),
            Answer::Permission(false)
        );
        assert_eq!(
            answer_from_values(&CardKind::Bypass, &approve),
            Answer::Permission(true)
        );
        assert_eq!(
            answer_from_values(&CardKind::Plan, &approve),
            Answer::Plan(true)
        );
        assert_eq!(
            answer_from_values(&CardKind::Plan, &reject),
            Answer::Plan(false)
        );
    }

    #[test]
    fn a_multi_answer_lists_labels_in_option_order() {
        let kind = question_card("?", &strings(&["Lint", "Unit", "E2E"]), true, false).kind;
        assert_eq!(
            answer_from_values(&kind, &strings(&["o3", "o1"])),
            question("Lint, E2E")
        );
        assert_eq!(
            answer_from_values(&kind, &strings(&["o2"])),
            question("Unit")
        );
        assert_eq!(
            answer_from_values(&kind, &strings(&["o9"])),
            Answer::Decline
        );
    }

    #[test]
    fn typed_yes_approves_and_anything_else_rejects() {
        for text in ["yes", " Y ", "YES"] {
            assert_eq!(
                answer_from_text(&CardKind::Permission, text),
                TypedOutcome::Answer(Answer::Permission(true))
            );
        }
        for text in ["no", "sure", ""] {
            assert_eq!(
                answer_from_text(&CardKind::Bypass, text),
                TypedOutcome::Answer(Answer::Permission(false))
            );
        }
        assert_eq!(
            answer_from_text(&CardKind::Plan, "yes"),
            TypedOutcome::Answer(Answer::Plan(false))
        );
    }

    #[test]
    fn typed_text_picks_options_by_label_or_number() {
        let options = strings(&["EU", "US", "Asia, Pacific"]);
        let single = question_card("?", &options, false, false).kind;
        let answer = |kind: &CardKind, text| answer_from_text(kind, text);
        assert_eq!(
            answer(&single, " eu "),
            TypedOutcome::Answer(question("EU"))
        );
        assert_eq!(answer(&single, "2"), TypedOutcome::Answer(question("US")));
        assert_eq!(
            answer(&single, "asia, pacific"),
            TypedOutcome::Answer(question("Asia, Pacific"))
        );
        assert_eq!(answer(&single, "4"), TypedOutcome::Retry(PICK_ONE));
        assert_eq!(answer(&single, "Mars"), TypedOutcome::Retry(PICK_ONE));
        assert_eq!(answer(&single, "  "), TypedOutcome::Retry(PICK_ONE));

        let multi = question_card("?", &options, true, false).kind;
        assert_eq!(
            answer(&multi, "us, 1"),
            TypedOutcome::Answer(question("EU, US"))
        );
        assert_eq!(answer(&multi, "us, Mars"), TypedOutcome::Retry(PICK_ONE));

        let free = question_card("?", &options, false, true).kind;
        assert_eq!(
            answer(&free, " Mars "),
            TypedOutcome::Answer(question("Mars"))
        );
        assert_eq!(answer(&free, "2"), TypedOutcome::Answer(question("US")));
        assert_eq!(answer(&free, ""), TypedOutcome::Retry(TYPE_ANSWER));
    }

    #[test]
    fn timeouts_reject_decline_or_leave_alone() {
        assert_eq!(
            timeout_answer(&CardKind::Permission),
            Some(Answer::Permission(false))
        );
        assert_eq!(timeout_answer(&CardKind::Plan), Some(Answer::Plan(false)));
        let kind = question_card("?", &strings(&["a"]), false, true).kind;
        assert_eq!(timeout_answer(&kind), Some(Answer::Decline));
        assert_eq!(timeout_answer(&CardKind::Bypass), None);
    }
}
