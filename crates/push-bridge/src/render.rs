//! A turn's output as `agent.reply@1` RICH bodies: the text in pages of 4,000 characters,
//! the latest thinking, and the tool calls as steps. Pure: the caller sends what `poll` and
//! `finish` return and passes the time in.

use std::time::Duration;

use push_domain::templates::{MAX_RICH_BYTES, check_value};
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::mask::{cap, mask, masked, masked_line};

/// Edits of a reply are at least this far apart.
pub const EDIT_INTERVAL: Duration = Duration::from_secs(2);
/// Characters of text per reply.
pub const TEXT_PAGE: usize = 4_000;
/// Characters of thinking shown: the latest ones.
pub const THINKING_TAIL: usize = 4_000;
/// Steps per reply.
pub const MAX_STEPS: usize = 50;
/// Characters per step label.
pub const STEP_LABEL: usize = 80;

/// Characters of a reply's title.
const TITLE: usize = 120;
/// Characters of a RICH body's fallback.
const FALLBACK: usize = 2_000;
/// Characters of the activity label.
const ACTIVITY_LABEL: usize = 60;
/// Characters of a subagent's task in a step label.
const TASK_PREFIX: usize = 30;
/// A page break looks this far back for a line break or a space.
const BREAK_WINDOW: usize = 200;
/// Page breaks a turn's text tries before it takes one that is not clean.
const MAX_BREAK_TRIES: usize = 64;
/// What an empty text shows.
const EMPTY: &str = "…";

/// One reply to send or edit.
#[derive(Clone, PartialEq, Eq)]
pub struct ReplyUpdate {
    /// The reply's 0-based index in this turn.
    pub page: usize,
    /// The `agent.reply@1` RICH body.
    pub body: String,
    /// Send a new reply; else edit the reply of this page.
    pub new: bool,
}

impl std::fmt::Debug for ReplyUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplyUpdate")
            .field("page", &self.page)
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field("new", &self.new)
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Done,
    Failed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

struct Step {
    /// The tool call's id; `None` for a task's step.
    id: Option<String>,
    label: String,
    /// The activity label while it runs: `<tool> <target>`.
    activity: Option<String>,
    started: Instant,
    status: Status,
    detail: Option<String>,
}

#[derive(Default)]
struct Page {
    /// The page's text once final.
    text: Option<String>,
    steps: Vec<Step>,
    /// The body last sent for this page.
    sent: Option<String>,
}

/// Renders one turn into `agent.reply@1` replies.
pub struct TurnRenderer {
    title: Option<String>,
    text: String,
    /// The next text starts on a new line (after a note).
    break_before_text: bool,
    thinking: String,
    pages: Vec<Page>,
    /// Characters of the raw (cleaned, unmasked) text in final pages.
    final_raw: usize,
    last_update: Instant,
    finished: bool,
}

impl TurnRenderer {
    pub fn new(now: Instant, title: Option<String>) -> Self {
        let title = title
            .map(|title| shown_line(&title, TITLE))
            .filter(|title| !title.trim().is_empty());
        Self {
            title,
            text: String::new(),
            break_before_text: false,
            thinking: String::new(),
            pages: vec![Page::default()],
            final_raw: 0,
            last_update: now,
            finished: false,
        }
    }

    pub fn thinking(&mut self, delta: &str) {
        self.thinking.push_str(delta);
        // Keeps twice the tail, so masking sees whole secrets around the cut.
        let keep = 2 * THINKING_TAIL;
        let count = self.thinking.chars().count();
        if count > 2 * keep {
            let start = self
                .thinking
                .char_indices()
                .nth(count - keep)
                .map_or(0, |(i, _)| i);
            self.thinking.drain(..start);
        }
    }

    pub fn text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if self.break_before_text {
            self.break_before_text = false;
            self.new_line();
        }
        self.text.push_str(delta);
    }

    pub fn tool_started(
        &mut self,
        id: &str,
        tool: &str,
        target: &str,
        task: Option<&str>,
        now: Instant,
    ) {
        let call = shown_line(&format!("{tool} {target}"), STEP_LABEL);
        let label = match task.map(|task| shown_line(task, TASK_PREFIX)) {
            Some(task) if !task.is_empty() => cap(&format!("{task}: {call}"), STEP_LABEL),
            _ => call,
        };
        let activity = shown_line(&format!("{tool} {target}"), ACTIVITY_LABEL);
        self.push_step(Step {
            id: Some(id.to_owned()),
            label,
            activity: Some(activity),
            started: now,
            status: Status::Running,
            detail: None,
        });
    }

    pub fn tool_finished(&mut self, id: &str, failed: bool, now: Instant) {
        let page = self.pages.last_mut().expect("a renderer has a page");
        let step = page
            .steps
            .iter_mut()
            .rev()
            .find(|step| step.status == Status::Running && step.id.as_deref() == Some(id));
        if let Some(step) = step {
            step.status = if failed { Status::Failed } else { Status::Done };
            step.detail = Some(duration(now.saturating_duration_since(step.started)));
        }
    }

    /// A background task's completion as a step in this reply.
    pub fn task_step(&mut self, label: &str, failed: bool) {
        let started = self.last_update;
        self.push_step(Step {
            id: None,
            label: shown_line(label, STEP_LABEL),
            activity: None,
            started,
            status: if failed { Status::Failed } else { Status::Done },
            detail: None,
        });
    }

    /// Appends `text` to the reply's text on its own line, e.g. "(Coda stopped; restarting)".
    pub fn note(&mut self, text: &str) {
        self.new_line();
        self.text.push_str(text);
        self.break_before_text = true;
    }

    /// What to send now: the first reply at once, then edits at most every
    /// [`EDIT_INTERVAL`] (only when something changed), plus a new reply each time the text
    /// passes another [`TEXT_PAGE`] characters (the previous page gets its final edit in the
    /// same call).
    pub fn poll(&mut self, now: Instant) -> Vec<ReplyUpdate> {
        if self.finished {
            return Vec::new();
        }
        let mut updates = Vec::new();
        let open = self.split(&mut updates);
        let page = self.pages.len() - 1;
        let body = self.render(page, &open);
        let sent = &mut self.pages[page].sent;
        let due = now.saturating_duration_since(self.last_update) >= EDIT_INTERVAL;
        let new = sent.is_none();
        if new || (due && sent.as_deref() != Some(body.as_str())) {
            *sent = Some(body.clone());
            self.last_update = now;
            updates.push(ReplyUpdate { page, body, new });
        }
        updates
    }

    /// The final state of every page that changed since its last update (interrupted: the
    /// text ends with "(stopped)"). Later calls return nothing.
    pub fn finish(&mut self, interrupted: bool) -> Vec<ReplyUpdate> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if interrupted {
            self.new_line();
            self.text.push_str("(stopped)");
            for page in &mut self.pages {
                for step in &mut page.steps {
                    if step.status == Status::Running {
                        step.status = Status::Failed;
                    }
                }
            }
        }
        let mut updates = Vec::new();
        let open = self.split(&mut updates);
        let page = self.pages.len() - 1;
        let body = self.render(page, &open);
        let sent = &mut self.pages[page].sent;
        if sent.as_deref() != Some(body.as_str()) {
            let new = sent.is_none();
            *sent = Some(body.clone());
            updates.push(ReplyUpdate { page, body, new });
        }
        updates
    }

    /// The activity label: the latest running tool as `<tool> <target>`, else "thinking".
    pub fn current_label(&self) -> String {
        self.pages
            .iter()
            .rev()
            .flat_map(|page| page.steps.iter().rev())
            .filter(|step| step.status == Status::Running)
            .find_map(|step| step.activity.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "thinking".to_owned())
    }

    fn new_line(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }

    fn push_step(&mut self, step: Step) {
        let steps = &mut self.pages.last_mut().expect("a renderer has a page").steps;
        steps.push(step);
        if steps.len() > MAX_STEPS {
            let oldest = steps
                .iter()
                .position(|step| step.status != Status::Running)
                .unwrap_or(0);
            steps.remove(oldest);
        }
    }

    /// Makes every full page final (its final body goes to `updates`; its running steps
    /// move on to the next page) and returns the open page's text. Pages are cut in the raw
    /// text and each is masked on its own: a final page is `mask(its raw text)`, the open
    /// page `mask(the raw text after the final pages)`, so text that arrives later never
    /// changes, loses or repeats a final page's text (see [`raw_page_break`]).
    fn split(&mut self, updates: &mut Vec<ReplyUpdate>) -> String {
        let all: Vec<char> = clean(&self.text, true).chars().collect();
        loop {
            let rest = all.get(self.final_raw..).unwrap_or_default();
            let open = mask(&string(rest));
            if open.chars().count() <= TEXT_PAGE {
                return open;
            }
            let at = raw_page_break(rest);
            let page = self.pages.len() - 1;
            let text = mask(&string(&rest[..at]));
            let (running, done) = std::mem::take(&mut self.pages[page].steps)
                .into_iter()
                .partition(|step| step.status == Status::Running);
            self.pages[page].steps = done;
            self.pages[page].text = Some(text.clone());
            let body = self.render(page, &text);
            let sent = &mut self.pages[page].sent;
            if sent.as_deref() != Some(body.as_str()) {
                let new = sent.is_none();
                *sent = Some(body.clone());
                updates.push(ReplyUpdate { page, body, new });
            }
            self.pages.push(Page {
                steps: running,
                ..Page::default()
            });
            self.final_raw += at;
        }
    }

    fn render(&self, page: usize, open: &str) -> String {
        let text = self.pages[page].text.as_deref().unwrap_or(open);
        let thinking = if page == 0 {
            let all: Vec<char> = mask(&clean(&self.thinking, true)).chars().collect();
            let tail: String = all[all.len().saturating_sub(THINKING_TAIL)..]
                .iter()
                .collect();
            Some(tail).filter(|tail| !tail.trim().is_empty())
        } else {
            None
        };
        let steps: Vec<Value> = self.pages[page]
            .steps
            .iter()
            .map(|step| {
                let mut item = json!({"label": step.label, "status": step.status.as_str()});
                if let Some(detail) = &step.detail {
                    item["detail"] = json!(detail);
                }
                item
            })
            .collect();
        reply_body(self.title.as_deref(), text, thinking.as_deref(), &steps)
    }
}

/// Where a final page of the raw (cleaned, unmasked) open text `rest` ends; `rest`
/// masked is longer than a page.
///
/// A page is final the moment it is cut and is masked on its own, so the cut must not fall
/// inside a secret. A cut goes after a whitespace character, never inside a run of other
/// characters: after a line break in the page's last [`BREAK_WINDOW`] characters, else
/// after a space there, else after any whitespace earlier in the page (latest first). Of
/// these (at most [`MAX_BREAK_TRIES`]), the first where
/// - the page masked fits [`TEXT_PAGE`] characters (a mask can be longer than what it
///   hides, as in `pwd=x`);
/// - something other than whitespace follows (so a `password: ` or `Bearer ` ending the
///   page already shows its value), and no secret pattern spans the cut: masking both
///   sides apart gives what masking them together gives;
/// - the page holds no `-----BEGIN` without its `-----END` (a key block still arriving).
///
/// When none passes, the latest cut whose masked page fits. When no cut fits (a page
/// without whitespace: a run longer than a page), the page is cut at [`TEXT_PAGE`], or
/// earlier until the masked page fits: each part is masked on its own, so a hex or base64
/// secret that long is still masked in both, unless the second part is shorter than the
/// 40 characters that mark one.
fn raw_page_break(rest: &[char]) -> usize {
    let size = TEXT_PAGE.min(rest.len());
    let window = size - BREAK_WINDOW.min(size);
    let newlines = (window + 1..=size).rev().filter(|&at| rest[at - 1] == '\n');
    let spaces = (window + 1..=size)
        .rev()
        .filter(|&at| rest[at - 1] != '\n' && rest[at - 1].is_whitespace());
    let earlier = (1..=window)
        .rev()
        .filter(|&at| rest[at - 1].is_whitespace());
    let mut fallback = None;
    for at in newlines.chain(spaces).chain(earlier).take(MAX_BREAK_TRIES) {
        if mask(&string(&rest[..at])).chars().count() > TEXT_PAGE {
            continue;
        }
        if clean_cut(rest, at) {
            return at;
        }
        fallback.get_or_insert(at);
    }
    if let Some(at) = fallback {
        return at;
    }
    let mut at = size;
    loop {
        let len = mask(&string(&rest[..at])).chars().count();
        if len <= TEXT_PAGE || at == 1 {
            return at;
        }
        at -= (len - TEXT_PAGE).min(at - 1);
    }
}

/// No secret spans a cut of `rest` at `at`, as far as the text so far shows.
fn clean_cut(rest: &[char], at: usize) -> bool {
    let page = string(&rest[..at]);
    let next = string(&rest[at..rest.len().min(at + TEXT_PAGE)]);
    if next.trim().is_empty() {
        return false;
    }
    if let Some(begin) = page.rfind("-----BEGIN") {
        if !page[begin..].contains("-----END") {
            return false;
        }
    }
    mask(&format!("{page}{next}")) == mask(&page) + &mask(&next)
}

fn string(chars: &[char]) -> String {
    chars.iter().collect()
}

/// `agent.reply@1` bodies for `text` (masked, then split into pages of [`TEXT_PAGE`]
/// characters), each titled `title`. Always at least one.
pub fn reply_bodies(title: Option<&str>, text: &str) -> Vec<String> {
    let title = title
        .map(|title| shown_line(title, TITLE))
        .filter(|title| !title.trim().is_empty());
    pages(&mask(&clean(text, true)))
        .iter()
        .map(|page| reply_body(title.as_deref(), page, None, &[]))
        .collect()
}

/// `text` (already masked) in pages of at most [`TEXT_PAGE`] characters; at least one.
pub(crate) fn pages(text: &str) -> Vec<String> {
    let mut rest: &[char] = &text.chars().collect::<Vec<_>>();
    let mut pages = Vec::new();
    loop {
        let at = page_break(rest, TEXT_PAGE);
        pages.push(rest[..at].iter().collect());
        rest = &rest[at..];
        if rest.is_empty() {
            return pages;
        }
    }
}

/// Where a page of at most `size` characters ends: after the last line break in its final
/// [`BREAK_WINDOW`] characters, else after the last space there, else at `size`.
fn page_break(text: &[char], size: usize) -> usize {
    if text.len() <= size {
        return text.len();
    }
    let window = &text[size - BREAK_WINDOW.min(size)..size];
    let after = |c: char| window.iter().rposition(|&x| x == c);
    match after('\n').or_else(|| after(' ')) {
        Some(i) => size - window.len() + i + 1,
        None => size,
    }
}

/// An `agent.reply@1` body. When it would pass the RICH size limit, the thinking goes
/// first, then most of the fallback, then the oldest steps.
pub(crate) fn reply_body(
    title: Option<&str>,
    text: &str,
    thinking: Option<&str>,
    steps: &[Value],
) -> String {
    let text = if text.trim().is_empty() { EMPTY } else { text };
    let mut thinking = thinking;
    let mut fallback = cap(text, FALLBACK);
    let mut steps = steps;
    loop {
        let mut params = Map::new();
        if let Some(title) = title {
            params.insert("title".into(), json!(title));
        }
        params.insert("text".into(), json!(text));
        if let Some(thinking) = thinking {
            params.insert("thinking".into(), json!(thinking));
        }
        if !steps.is_empty() {
            params.insert("steps".into(), json!(steps));
        }
        let body = rich_body("agent.reply@1", params, &fallback);
        if body.len() <= MAX_RICH_BYTES {
            return body;
        }
        if thinking.is_some() {
            thinking = None;
        } else if fallback.chars().count() > 200 {
            fallback = cap(text, 200);
        } else if !steps.is_empty() {
            steps = &steps[steps.len().div_ceil(2)..];
        } else {
            return body;
        }
    }
}

/// `{"tpl": …, "params": …, "fallback": …}`.
pub(crate) fn rich_body(tpl: &str, params: Map<String, Value>, fallback: &str) -> String {
    let fallback = if fallback.trim().is_empty() {
        EMPTY
    } else {
        fallback
    };
    json!({"tpl": tpl, "params": params, "fallback": fallback}).to_string()
}

/// Only characters a body's string value may hold: line breaks become `\n` (or a space on
/// one line), tabs four spaces, and other control and invisible characters go.
pub(crate) fn clean(text: &str, multiline: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut buffer = [0u8; 4];
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(if multiline { '\n' } else { ' ' });
            }
            '\n' | '\u{2028}' | '\u{2029}' => out.push(if multiline { '\n' } else { ' ' }),
            '\t' => out.push_str("    "),
            c if check_value(c.encode_utf8(&mut buffer), 1, false).is_ok() => out.push(c),
            _ => {}
        }
    }
    out
}

/// Cleaned, masked and capped multi-line text.
pub(crate) fn shown(text: &str, max: usize) -> String {
    masked(&clean(text, true), max)
}

/// Cleaned, masked and capped single-line text.
pub(crate) fn shown_line(text: &str, max: usize) -> String {
    masked_line(&clean(text, false), max)
}

/// `1.2 s`, `2 min 5 s`, `1 h 3 min`.
fn duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        let tenths = elapsed.as_millis() / 100;
        format!("{}.{} s", tenths / 10, tenths % 10)
    } else if secs < 3600 {
        match secs % 60 {
            0 => format!("{} min", secs / 60),
            s => format!("{} min {s} s", secs / 60),
        }
    } else {
        format!("{} h {} min", secs / 3600, secs % 3600 / 60)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use push_domain::templates::{ListItem, ParamValue, RichBody, builtins};

    use super::*;

    /// Checks `body` against the built-in `tpl` and returns it parsed.
    pub(crate) fn valid(body: &str, tpl: &str) -> RichBody {
        let definition = builtins()
            .into_iter()
            .find(|d| d.template_ref().to_string() == tpl)
            .expect("a built-in template");
        let parsed = RichBody::parse(body.as_bytes()).expect("a RICH body");
        parsed
            .check(&definition)
            .expect("the body fits its template");
        parsed
    }

    pub(crate) fn param(body: &RichBody, name: &str) -> Option<String> {
        match body.params.get(name) {
            Some(ParamValue::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    fn reply(update: &ReplyUpdate) -> RichBody {
        valid(&update.body, "agent.reply@1")
    }

    fn steps(body: &RichBody) -> Vec<(String, String, Option<String>)> {
        match body.params.get("steps") {
            Some(ParamValue::List(items)) => items
                .iter()
                .map(|item| match item {
                    ListItem::Step {
                        label,
                        status,
                        detail,
                    } => (label.clone(), status.as_str().to_owned(), detail.clone()),
                    _ => panic!("not a step"),
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn secs(n: f64) -> Duration {
        Duration::from_secs_f64(n)
    }

    #[test]
    fn the_first_poll_sends_page_0_at_once() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, Some("Fix the build".into()));
        let updates = renderer.poll(t0);
        assert_eq!(updates.len(), 1);
        assert_eq!((updates[0].page, updates[0].new), (0, true));
        let body = reply(&updates[0]);
        assert_eq!(param(&body, "text").as_deref(), Some("…"));
        assert_eq!(param(&body, "title").as_deref(), Some("Fix the build"));
        assert_eq!(body.fallback, "…");
    }

    #[test]
    fn edits_wait_for_the_interval_and_a_change() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.poll(t0);
        renderer.text("Hello");
        assert!(renderer.poll(t0 + secs(1.0)).is_empty());
        renderer.text(" world");
        assert!(renderer.poll(t0 + secs(1.9)).is_empty());
        let updates = renderer.poll(t0 + secs(2.0));
        assert_eq!(updates.len(), 1);
        assert_eq!((updates[0].page, updates[0].new), (0, false));
        assert_eq!(
            param(&reply(&updates[0]), "text").as_deref(),
            Some("Hello world")
        );
        assert!(renderer.poll(t0 + secs(10.0)).is_empty());
    }

    #[test]
    fn thinking_keeps_the_latest_4000_characters() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        for _ in 0..30 {
            renderer.thinking(&"a b ".repeat(250));
        }
        renderer.thinking("THE END");
        let body = reply(&renderer.poll(t0)[0]);
        let thinking = param(&body, "thinking").unwrap();
        assert_eq!(thinking.chars().count(), THINKING_TAIL);
        assert!(thinking.ends_with("a b THE END"));
    }

    #[test]
    fn no_thinking_means_no_thinking_param() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.thinking(" \n ");
        let body = reply(&renderer.poll(t0)[0]);
        assert_eq!(body.params.get("thinking"), None);
        assert_eq!(body.params.get("steps"), None);
    }

    #[test]
    fn sixty_tools_leave_fifty_steps() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        for i in 0..60 {
            renderer.tool_started(&format!("c{i}"), "view", &format!("f{i}.rs"), None, t0);
            renderer.tool_finished(&format!("c{i}"), false, t0 + secs(1.0));
        }
        let steps = steps(&reply(&renderer.poll(t0)[0]));
        assert_eq!(steps.len(), MAX_STEPS);
        assert_eq!(steps[0].0, "view f10.rs");
        assert_eq!(steps[49].0, "view f59.rs");
    }

    #[test]
    fn the_oldest_finished_steps_go_first() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.tool_started("long", "bash", "cargo build", None, t0);
        for i in 0..55 {
            renderer.tool_started(&format!("c{i}"), "view", "x", None, t0);
            renderer.tool_finished(&format!("c{i}"), false, t0);
        }
        let steps = steps(&reply(&renderer.poll(t0)[0]));
        assert_eq!(steps.len(), MAX_STEPS);
        assert_eq!(steps[0].0, "bash cargo build");
        assert_eq!(steps[0].1, "running");
    }

    #[test]
    fn a_step_runs_then_finishes_with_its_duration() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.tool_started("c1", "bash", "cargo test", None, t0);
        renderer.tool_started("c2", "edit", "src/lib.rs", None, t0);
        assert_eq!(
            steps(&reply(&renderer.poll(t0)[0]))[0],
            ("bash cargo test".into(), "running".into(), None)
        );
        assert_eq!(renderer.current_label(), "edit src/lib.rs");
        renderer.tool_finished("c1", false, t0 + secs(1.25));
        renderer.tool_finished("c2", true, t0 + secs(125.0));
        assert_eq!(renderer.current_label(), "thinking");
        let steps = steps(&reply(&renderer.poll(t0 + secs(130.0))[0]));
        assert_eq!(
            steps,
            [
                (
                    "bash cargo test".to_owned(),
                    "done".to_owned(),
                    Some("1.2 s".to_owned())
                ),
                (
                    "edit src/lib.rs".to_owned(),
                    "failed".to_owned(),
                    Some("2 min 5 s".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn a_subagent_step_names_its_task() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        let long_target = "ab ".repeat(70);
        renderer.tool_started("c1", "grep", "TODO", Some("Find the callers"), t0);
        renderer.tool_started("c2", "view", &long_target, Some("Review"), t0);
        let steps = steps(&reply(&renderer.poll(t0)[0]));
        assert_eq!(steps[0].0, "Find the callers: grep TODO");
        assert!(steps[1].0.starts_with("Review: view ab ab"));
        assert_eq!(steps[1].0.chars().count(), STEP_LABEL);
        assert_eq!(renderer.current_label().chars().count(), ACTIVITY_LABEL);
    }

    #[test]
    fn a_background_task_becomes_a_step() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.task_step("Task Run the tests completed", false);
        renderer.task_step("Task Lint failed", true);
        let steps = steps(&reply(&renderer.poll(t0)[0]));
        assert_eq!(
            steps[0],
            ("Task Run the tests completed".into(), "done".into(), None)
        );
        assert_eq!(steps[1].1, "failed");
    }

    #[test]
    fn long_text_continues_in_new_pages_and_earlier_pages_are_final() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.poll(t0);
        renderer.thinking("Planning");
        renderer.tool_started("c1", "bash", "make", None, t0);
        let line = format!("{}word\n", "word ".repeat(19));
        for _ in 0..90 {
            renderer.text(&line);
        }
        let updates = renderer.poll(t0 + secs(0.5));
        let pages: Vec<(usize, bool)> = updates.iter().map(|u| (u.page, u.new)).collect();
        assert_eq!(pages, [(0, false), (1, true), (2, true)]);
        let first = reply(&updates[0]);
        let text0 = param(&first, "text").unwrap();
        assert!(text0.chars().count() <= TEXT_PAGE);
        assert!(text0.ends_with('\n'));
        assert_eq!(param(&first, "thinking").as_deref(), Some("Planning"));
        assert_eq!(first.params.get("steps"), None);
        let second = reply(&updates[1]);
        assert_eq!(second.params.get("thinking"), None);
        assert_eq!(steps(&second), []);
        let third = reply(&updates[2]);
        assert_eq!(steps(&third)[0].1, "running");
        let total: usize = updates
            .iter()
            .map(|u| param(&reply(u), "text").unwrap().chars().count())
            .sum();
        assert_eq!(total, 9_000);

        renderer.text("more");
        renderer.tool_finished("c1", false, t0 + secs(3.0));
        let later = renderer.poll(t0 + secs(3.0));
        assert_eq!(later.len(), 1);
        assert_eq!((later[0].page, later[0].new), (2, false));
        let finals = renderer.finish(false);
        assert!(finals.is_empty());
        assert!(renderer.poll(t0 + secs(10.0)).is_empty());
    }

    /// Streams `chunks`, polling after each, then finishes; returns every update's page
    /// and text, and each page's last text.
    fn stream(chunks: &[String]) -> (Vec<(usize, String)>, Vec<String>) {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        let mut updates = renderer.poll(t0);
        for (i, chunk) in chunks.iter().enumerate() {
            renderer.text(chunk);
            updates.extend(renderer.poll(t0 + secs(3.0 * (i + 1) as f64)));
        }
        updates.extend(renderer.finish(false));
        let texts: Vec<(usize, String)> = updates
            .iter()
            .map(|u| (u.page, param(&reply(u), "text").unwrap()))
            .collect();
        let pages = texts.iter().map(|(page, _)| page + 1).max().unwrap_or(0);
        let last = (0..pages)
            .map(|page| {
                texts
                    .iter()
                    .rev()
                    .find(|(p, _)| *p == page)
                    .map(|(_, text)| text.clone())
                    .unwrap()
            })
            .collect();
        (texts, last)
    }

    fn hex_run(text: &str) -> bool {
        text.chars()
            .collect::<Vec<_>>()
            .windows(4)
            .any(|w| w.iter().all(|c| c.is_ascii_hexdigit()))
    }

    #[test]
    fn a_hex_token_across_a_page_break_never_shows_in_a_final_page() {
        let filler = "xyz ".repeat(925);
        let hex = "0123456789abcdef".repeat(4);
        let chunks = [
            filler,
            format!("{}{}", "=".repeat(262), &hex[..39]),
            format!("{} done", &hex[39..]),
        ];
        let (_, pages) = stream(&chunks);
        assert_eq!(pages.len(), 2);
        for page in &pages {
            assert!(!hex_run(page), "{page}");
            assert!(page.chars().count() <= TEXT_PAGE);
        }
        assert!(pages[1].ends_with("«masked» done"), "{}", pages[1]);
    }

    #[test]
    fn a_key_block_arriving_across_a_page_break_is_masked_and_nothing_is_lost() {
        let chunks = [
            "xyz ".repeat(995),
            "-----BEGIN OPENSSH PRIV".to_owned(),
            "ATE KEY-----\n".to_owned(),
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n".to_owned(),
            "QyNTUxOQAAACD\n".to_owned(),
            "-----END OPENSSH PRIVATE KEY-----\nafter\n".to_owned(),
        ];
        let (updates, pages) = stream(&chunks);
        for (_, text) in &updates {
            assert!(!text.contains("b3Bl") && !text.contains("QyNT"), "{text}");
        }
        assert_eq!(pages.concat(), mask(&chunks.concat()));
        assert!(pages.concat().ends_with("«masked»\nafter\n"));
    }

    #[test]
    fn streamed_pages_lose_and_repeat_nothing() {
        let line = |i: usize| {
            format!(
                "line {i}: api_key=k{i} pwd=x sha {}{i:04} and some words\n",
                "f".repeat(40)
            )
        };
        let text: String = (0..400).map(line).collect();
        let chunks: Vec<String> = text
            .chars()
            .collect::<Vec<_>>()
            .chunks(37)
            .map(|chunk| chunk.iter().collect())
            .collect();
        let (updates, pages) = stream(&chunks);
        assert!(pages.len() > 2);
        for (_, text) in &updates {
            assert!(text.chars().count() <= TEXT_PAGE);
        }
        assert_eq!(pages.concat(), mask(&text));

        let plain: String = (0..2_000).map(|i| format!("w{i} ")).collect();
        let chunks: Vec<String> = plain
            .chars()
            .collect::<Vec<_>>()
            .chunks(53)
            .map(|chunk| chunk.iter().collect())
            .collect();
        assert_eq!(stream(&chunks).1.concat(), plain);
    }

    #[test]
    fn a_page_without_a_break_is_cut_at_the_limit() {
        assert_eq!(pages(&"x".repeat(8_001)).len(), 3);
        assert_eq!(pages(&"x".repeat(8_001))[0].chars().count(), TEXT_PAGE);
        assert_eq!(pages(""), [""]);
    }

    #[test]
    fn secrets_in_text_and_targets_are_masked() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.text("the password: ");
        renderer.text("hunter2 is set");
        renderer.tool_started(
            "c1",
            "bash",
            "curl -H 'authorization: bearer t0k3n'",
            None,
            t0,
        );
        let body = reply(&renderer.poll(t0)[0]);
        let text = param(&body, "text").unwrap();
        assert_eq!(text, "the password: «masked» is set");
        assert!(!body.fallback.contains("hunter2"));
        let label = &steps(&body)[0].0;
        assert!(!label.contains("t0k3n"), "{label}");
        assert!(!renderer.current_label().contains("t0k3n"));
    }

    #[test]
    fn control_characters_never_reach_a_body() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, Some("a\tb\u{1b}[0m".into()));
        renderer.text("red \u{1b}[31mtext\r\nnext\u{200B}line\u{2028}end\ttab");
        renderer.thinking("x\u{0}y");
        renderer.tool_started("c1", "bash\n", "echo\u{7}", None, t0);
        let body = reply(&renderer.poll(t0)[0]);
        assert_eq!(
            param(&body, "text").as_deref(),
            Some("red [31mtext\nnextline\nend    tab")
        );
    }

    #[test]
    fn an_interrupted_turn_ends_with_stopped() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.poll(t0);
        renderer.text("Working on it");
        renderer.tool_started("c1", "bash", "sleep 100", None, t0);
        let updates = renderer.finish(true);
        assert_eq!(updates.len(), 1);
        assert!(!updates[0].new);
        let body = reply(&updates[0]);
        assert_eq!(
            param(&body, "text").as_deref(),
            Some("Working on it\n(stopped)")
        );
        assert_eq!(steps(&body)[0].1, "failed");
        assert!(renderer.finish(true).is_empty());
    }

    #[test]
    fn finish_sends_an_unsent_reply() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.text("Done.");
        let updates = renderer.finish(false);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].new);
        assert_eq!(param(&reply(&updates[0]), "text").as_deref(), Some("Done."));
    }

    #[test]
    fn a_note_stands_on_its_own_line() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.text("partial");
        renderer.note("(Coda stopped; restarting)");
        renderer.text("again");
        let body = reply(&renderer.poll(t0)[0]);
        assert_eq!(
            param(&body, "text").as_deref(),
            Some("partial\n(Coda stopped; restarting)\nagain")
        );
    }

    #[test]
    fn an_oversized_body_drops_thinking_first() {
        let t0 = Instant::now();
        let mut renderer = TurnRenderer::new(t0, None);
        renderer.text(&"界".repeat(3_999));
        renderer.thinking(&"界".repeat(3_999));
        for i in 0..50 {
            renderer.tool_started(&format!("c{i}"), "界", &"界".repeat(100), None, t0);
        }
        let update = &renderer.poll(t0)[0];
        assert!(update.body.len() <= MAX_RICH_BYTES);
        let body = reply(update);
        assert_eq!(body.params.get("thinking"), None);
        assert_eq!(steps(&body).len(), MAX_STEPS);
    }

    #[test]
    fn reply_bodies_page_long_text() {
        let bodies = reply_bodies(Some("Plan"), &"line\n".repeat(1_700));
        assert_eq!(bodies.len(), 3);
        for body in &bodies {
            let body = valid(body, "agent.reply@1");
            assert_eq!(param(&body, "title").as_deref(), Some("Plan"));
        }
        assert_eq!(reply_bodies(None, "").len(), 1);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(secs(0.04)), "0.0 s");
        assert_eq!(duration(secs(59.99)), "59.9 s");
        assert_eq!(duration(secs(120.0)), "2 min");
        assert_eq!(duration(secs(3_725.0)), "1 h 2 min");
    }
}
