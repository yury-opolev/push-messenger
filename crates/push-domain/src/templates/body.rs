//! RICH and ANSWER message bodies.
//!
//! A RICH body fills a template: `{"tpl": "<ref>", "params": {...}, "fallback": "..."}`.
//! An ANSWER body answers a card, a RICH message with an interactive block:
//! `{"card": "<message uuid>", "values": ["<option id>"], "fallback": "..."}`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::strict::{self, Object, StrictValue, at, join};
use super::wording::check_value;
use super::{
    Block, Choices, Definition, ItemType, MAX_ANSWER_BYTES, MAX_RICH_BYTES, ParamType,
    TemplateError, TemplateRef, is_key,
};

const MAX_RICH_FALLBACK_CHARS: usize = 2000;
const MAX_ANSWER_FALLBACK_CHARS: usize = 500;
const MAX_ANSWER_VALUES: usize = 10;
const MAX_OPTION_LABEL_CHARS: usize = 80;
const MAX_STEP_LABEL_CHARS: usize = 200;
const MAX_STEP_DETAIL_CHARS: usize = 1000;

/// A RICH message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichBody {
    pub template: TemplateRef,
    pub params: Params,
    /// Plain text for clients that cannot render the template.
    pub fallback: String,
}

/// A body's param values, in body order, names unique.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(Vec<(String, ParamValue)>);

/// A param value. An `enum` value is a `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamValue {
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<ListItem>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListItem {
    String(String),
    Option {
        id: String,
        label: String,
    },
    Step {
        label: String,
        status: StepStatus,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

/// An ANSWER message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerBody {
    /// The message id of the card answered.
    pub card: Uuid,
    /// The option ids chosen, distinct.
    pub values: Vec<String>,
    /// Plain text for clients that cannot render the answer.
    pub fallback: String,
}

impl Params {
    pub fn get(&self, name: &str) -> Option<&ParamValue> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ParamValue)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v))
    }
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

impl RichBody {
    /// Reads the envelope: at most `MAX_RICH_BYTES` of strict JSON holding exactly `tpl`
    /// (a valid ref), `params` (an object of strings, integers, booleans and lists of
    /// strings, options and steps) and `fallback` (1 to 2000 characters, not only
    /// whitespace). The params are checked against a definition by `check`.
    pub fn parse(body: &[u8]) -> Result<RichBody, TemplateError> {
        let value = parse_document(body, "RICH", MAX_RICH_BYTES)?;
        let root = value.object("")?;
        root.only(&["tpl", "params", "fallback"])?;
        let template = root
            .string("tpl")?
            .parse::<TemplateRef>()
            .map_err(|e| at("tpl", e))?;
        let params = root
            .field("params")?
            .object("params")?
            .fields()
            .iter()
            .map(|(name, value)| Ok((name.clone(), parse_value(value, &join("params", name))?)))
            .collect::<Result<_, TemplateError>>()?;
        let fallback = fallback(&root, MAX_RICH_FALLBACK_CHARS, true)?;
        Ok(RichBody {
            template,
            params: Params(params),
            fallback,
        })
    }

    /// Checks the params against the definition's schema: every required param present,
    /// none unknown, each value of its declared type and within its caps. A list bound to
    /// `single` or `multi` must offer enough options to answer.
    pub fn check(&self, definition: &Definition) -> Result<(), TemplateError> {
        same_template(&self.template, definition)?;
        if let Some((name, _)) = self
            .params
            .iter()
            .find(|(n, _)| definition.param(n).is_none())
        {
            return Err(at(
                &join("params", name),
                format!("is not a param of {}", self.template),
            ));
        }
        for (name, spec) in definition.params() {
            let path = join("params", name);
            match self.params.get(name) {
                Some(value) => check_param(value, &spec.ty, &path)?,
                None if spec.optional => {}
                None => return Err(at(&path, "is required")),
            }
        }
        let needed = match definition.interactive() {
            Some(Block::Single {
                choices: Choices::Param(p),
                ..
            }) => Some((p, 1)),
            Some(Block::Multi {
                choices: Choices::Param(p),
                min,
                ..
            }) => Some((p, *min as usize)),
            _ => None,
        };
        if let Some((name, needed)) = needed {
            let offered = self.offered(definition).len();
            if offered < needed {
                return Err(at(
                    &join("params", name),
                    format!("offers {offered} options; the card needs at least {needed}"),
                ));
            }
        }
        Ok(())
    }

    /// The option ids the definition's interactive block offers on this card: its literal
    /// ids, or the ids in the card's own `list` of `option` param. Empty without one.
    pub fn offered(&self, definition: &Definition) -> Vec<String> {
        let choices = match definition.interactive() {
            Some(Block::Buttons { options }) => {
                return options.iter().map(|o| o.id.clone()).collect();
            }
            Some(Block::Single { choices, .. } | Block::Multi { choices, .. }) => choices,
            _ => return Vec::new(),
        };
        match choices {
            Choices::Literal(options) => options.iter().map(|o| o.id.clone()).collect(),
            Choices::Param(name) => match self.params.get(name) {
                Some(ParamValue::List(items)) => items
                    .iter()
                    .filter_map(|item| match item {
                        ListItem::Option { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            },
        }
    }
}

impl AnswerBody {
    /// Reads the body: at most `MAX_ANSWER_BYTES` of strict JSON holding exactly `card` (a
    /// hyphenated UUID), `values` (1 to 10 distinct option ids, `^[a-z][a-z0-9_]{0,31}$`) and
    /// `fallback` (1 to 500 characters on one line, not only whitespace).
    pub fn parse(body: &[u8]) -> Result<AnswerBody, TemplateError> {
        let value = parse_document(body, "ANSWER", MAX_ANSWER_BYTES)?;
        let root = value.object("")?;
        root.only(&["card", "values", "fallback"])?;
        let card = root.string("card")?;
        let card = Uuid::try_parse(card)
            .ok()
            .filter(|_| card.len() == 36)
            .ok_or_else(|| at("card", format!("{card:?} is not a hyphenated UUID")))?;
        let items = root.array("values")?;
        if !(1..=MAX_ANSWER_VALUES).contains(&items.len()) {
            return Err(at(
                "values",
                format!("needs 1 to {MAX_ANSWER_VALUES} values, not {}", items.len()),
            ));
        }
        let mut values: Vec<String> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let path = format!("values[{i}]");
            let value = item.string(&path)?;
            check_option_id(value, &path)?;
            if values.iter().any(|v| v == value) {
                return Err(at(&path, format!("{value:?} is listed twice")));
            }
            values.push(value.to_owned());
        }
        let fallback = fallback(&root, MAX_ANSWER_FALLBACK_CHARS, false)?;
        Ok(AnswerBody {
            card,
            values,
            fallback,
        })
    }

    /// Checks the answer against its card and the card's definition: every value is an
    /// option the card offers; exactly one for `buttons` and `single`, `min` to `max` for
    /// `multi`.
    pub fn check(&self, card: &RichBody, definition: &Definition) -> Result<(), TemplateError> {
        same_template(&card.template, definition)?;
        let (min, max) = match definition.interactive() {
            Some(Block::Multi { min, max, .. }) => (*min as usize, *max as usize),
            Some(_) => (1, 1),
            None => {
                return Err(TemplateError(format!(
                    "{} has no interactive block to answer",
                    card.template
                )));
            }
        };
        let count = self.values.len();
        if !(min..=max).contains(&count) {
            let expected = if min == max {
                format!("exactly {min} value{}", if min == 1 { "" } else { "s" })
            } else {
                format!("{min} to {max} values")
            };
            return Err(at(
                "values",
                format!("the card takes {expected}, not {count}"),
            ));
        }
        let offered = card.offered(definition);
        for (i, value) in self.values.iter().enumerate() {
            if !offered.contains(value) {
                return Err(at(
                    &format!("values[{i}]"),
                    format!("{value:?} is not an option of the card"),
                ));
            }
        }
        Ok(())
    }
}

fn parse_document(body: &[u8], kind: &str, limit: usize) -> Result<StrictValue, TemplateError> {
    if body.len() > limit {
        return Err(TemplateError(format!(
            "the {kind} body is {} bytes; the limit is {limit}",
            body.len()
        )));
    }
    if std::str::from_utf8(body).is_err() {
        return Err(TemplateError(format!("the {kind} body is not valid UTF-8")));
    }
    let value = strict::parse(body)
        .map_err(|e| TemplateError(format!("the {kind} body is not strict JSON: {e}")))?;
    match value {
        StrictValue::Object(_) => Ok(value),
        _ => Err(TemplateError(format!(
            "the {kind} body must be a JSON object"
        ))),
    }
}

fn same_template(template: &TemplateRef, definition: &Definition) -> Result<(), TemplateError> {
    let expected = definition.template_ref();
    if *template != expected {
        return Err(at(
            "tpl",
            format!("names {template}, but the definition is {expected}"),
        ));
    }
    Ok(())
}

fn fallback(root: &Object<'_>, max_chars: usize, multiline: bool) -> Result<String, TemplateError> {
    let text = root.string("fallback")?;
    if text.trim().is_empty() {
        return Err(at("fallback", "is empty"));
    }
    check_value(text, max_chars, multiline).map_err(|e| at("fallback", e))?;
    Ok(text.to_owned())
}

fn parse_value(value: &StrictValue, path: &str) -> Result<ParamValue, TemplateError> {
    match value {
        StrictValue::String(s) => Ok(ParamValue::String(s.clone())),
        StrictValue::Bool(b) => Ok(ParamValue::Bool(*b)),
        StrictValue::Number(_) => value.integer(path).map(ParamValue::Int),
        StrictValue::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| parse_item(item, &format!("{path}[{i}]")))
            .collect::<Result<_, _>>()
            .map(ParamValue::List),
        _ => Err(at(
            path,
            "must be a string, an integer, a boolean or an array",
        )),
    }
}

fn parse_item(value: &StrictValue, path: &str) -> Result<ListItem, TemplateError> {
    if let StrictValue::String(s) = value {
        return Ok(ListItem::String(s.clone()));
    }
    let shape = "must be a string, an option {id, label} or a step {label, status, detail?}";
    let StrictValue::Object(_) = value else {
        return Err(at(path, shape));
    };
    let obj = value.object(path)?;
    if obj.get("id").is_some() {
        obj.only(&["id", "label"])?;
        return Ok(ListItem::Option {
            id: obj.string("id")?.to_owned(),
            label: obj.string("label")?.to_owned(),
        });
    }
    if obj.get("status").is_none() {
        return Err(at(path, shape));
    }
    obj.only(&["label", "status", "detail"])?;
    let status = match obj.string("status")? {
        "pending" => StepStatus::Pending,
        "running" => StepStatus::Running,
        "done" => StepStatus::Done,
        "failed" => StepStatus::Failed,
        "skipped" => StepStatus::Skipped,
        other => {
            return Err(at(
                &obj.path("status"),
                format!("must be pending, running, done, failed or skipped, not {other:?}"),
            ));
        }
    };
    let detail = match obj.get("detail") {
        Some(detail) => Some(detail.string(&obj.path("detail"))?.to_owned()),
        None => None,
    };
    Ok(ListItem::Step {
        label: obj.string("label")?.to_owned(),
        status,
        detail,
    })
}

fn check_param(value: &ParamValue, ty: &ParamType, path: &str) -> Result<(), TemplateError> {
    match (ty, value) {
        (ParamType::String { max_len, multiline }, ParamValue::String(s)) => {
            check_label(s, *max_len as usize, *multiline, path)
        }
        (ParamType::Enum { values }, ParamValue::String(s)) => {
            if values.contains(s) {
                Ok(())
            } else {
                Err(at(
                    path,
                    format!("{s:?} is not one of {}", values.join(", ")),
                ))
            }
        }
        (ParamType::Int { min, max }, ParamValue::Int(n)) => {
            if (min..=max).contains(&n) {
                Ok(())
            } else {
                Err(at(path, format!("must be {min} to {max}, not {n}")))
            }
        }
        (ParamType::Bool, ParamValue::Bool(_)) => Ok(()),
        (ParamType::List { item, max_items }, ParamValue::List(items)) => {
            if items.len() > *max_items as usize {
                return Err(at(
                    path,
                    format!("has {} items; the limit is {max_items}", items.len()),
                ));
            }
            let mut ids: Vec<&str> = Vec::new();
            for (i, value) in items.iter().enumerate() {
                let item_path = format!("{path}[{i}]");
                check_item(value, item, &item_path)?;
                if let ListItem::Option { id, .. } = value {
                    if ids.contains(&id.as_str()) {
                        return Err(at(
                            &join(&item_path, "id"),
                            format!("duplicate option id {id:?}"),
                        ));
                    }
                    ids.push(id);
                }
            }
            Ok(())
        }
        _ => Err(at(path, format!("must be {}", describe(ty)))),
    }
}

fn describe(ty: &ParamType) -> &'static str {
    match ty {
        ParamType::String { .. } => "a string",
        ParamType::Enum { .. } => "a string naming an enum value",
        ParamType::Int { .. } => "an integer",
        ParamType::Bool => "a boolean",
        ParamType::List { .. } => "an array",
    }
}

/// Option ids in a body: `^[a-z][a-z0-9_]{0,31}$`, as everywhere else.
fn check_option_id(id: &str, path: &str) -> Result<(), TemplateError> {
    if is_key(id) {
        Ok(())
    } else {
        Err(at(
            path,
            format!("{id:?} does not match ^[a-z][a-z0-9_]{{0,31}}$"),
        ))
    }
}

fn check_text(
    text: &str,
    max_chars: usize,
    multiline: bool,
    path: &str,
) -> Result<(), TemplateError> {
    check_value(text, max_chars, multiline).map_err(|e| at(path, e))
}

/// A present string: like `check_text`, and not empty or only whitespace.
fn check_label(
    text: &str,
    max_chars: usize,
    multiline: bool,
    path: &str,
) -> Result<(), TemplateError> {
    if text.trim().is_empty() {
        return Err(at(path, "is empty"));
    }
    check_text(text, max_chars, multiline, path)
}

fn check_item(value: &ListItem, ty: &ItemType, path: &str) -> Result<(), TemplateError> {
    match (ty, value) {
        (ItemType::String { max_len }, ListItem::String(s)) => {
            check_label(s, *max_len as usize, false, path)
        }
        (ItemType::Option, ListItem::Option { id, label }) => {
            check_option_id(id, &join(path, "id"))?;
            check_label(label, MAX_OPTION_LABEL_CHARS, false, &join(path, "label"))
        }
        (ItemType::Step, ListItem::Step { label, detail, .. }) => {
            check_label(label, MAX_STEP_LABEL_CHARS, false, &join(path, "label"))?;
            match detail {
                Some(detail) => {
                    check_text(detail, MAX_STEP_DETAIL_CHARS, true, &join(path, "detail"))
                }
                None => Ok(()),
            }
        }
        (ItemType::String { .. }, _) => Err(at(path, "must be a string")),
        (ItemType::Option, _) => Err(at(path, "must be an option {id, label}")),
        (ItemType::Step, _) => Err(at(path, "must be a step {label, status, detail?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::builtins;
    use super::*;
    use serde_json::{Value, json};

    fn builtin(name: &str) -> Definition {
        builtins()
            .into_iter()
            .find(|d| d.template_ref().name == name)
            .unwrap()
    }

    fn rich(value: &Value) -> Result<RichBody, TemplateError> {
        RichBody::parse(&serde_json::to_vec(value).unwrap())
    }

    fn checked(value: &Value, definition: &Definition) -> Result<RichBody, TemplateError> {
        let body = rich(value)?;
        body.check(definition)?;
        Ok(body)
    }

    fn refused(result: Result<impl std::fmt::Debug, TemplateError>, expected: &str) {
        let error = result.expect_err("the body should be refused").0;
        assert!(error.contains(expected), "{error:?} lacks {expected:?}");
    }

    fn approval() -> Value {
        json!({
            "tpl": "agent.approval@1",
            "params": {"title": "Deploy", "details": "Ship build 42\nto prod?", "preview": "<b>diff</b> https://x.io"},
            "fallback": "Approve deploying build 42?"
        })
    }

    fn question() -> Value {
        json!({
            "tpl": "agent.question@1",
            "params": {
                "question": "Which region?",
                "options": [{"id": "eu_west", "label": "EU"}, {"id": "us_1", "label": "US"}]
            },
            "fallback": "Which region: EU or US?"
        })
    }

    fn answer(card: &str, values: &[&str]) -> Result<AnswerBody, TemplateError> {
        AnswerBody::parse(
            &serde_json::to_vec(&json!({"card": card, "values": values, "fallback": "Done"}))
                .unwrap(),
        )
    }

    const CARD: &str = "0190f5d2-7c3a-7b4e-9a1f-2c3d4e5f6a7b";

    fn custom(params: Value, layout: Value) -> Definition {
        let value = json!({
            "format": "tpl/1", "publisher": "acme", "name": "custom", "version": 1,
            "title": "Custom", "senders": ["human"], "params": params, "layout": layout
        });
        Definition::parse(&serde_json::to_vec(&value).unwrap()).unwrap()
    }

    fn custom_body(params: Value) -> Value {
        json!({"tpl": "acme.custom@1", "params": params, "fallback": "Custom"})
    }

    #[test]
    fn a_valid_approval_passes() {
        let body = checked(&approval(), &builtin("approval")).unwrap();
        assert_eq!(body.template.to_string(), "agent.approval@1");
        assert_eq!(body.params.len(), 3);
        assert_eq!(
            body.params.get("title"),
            Some(&ParamValue::String("Deploy".into()))
        );
        assert_eq!(body.offered(&builtin("approval")), ["approve", "reject"]);
        let mut without_preview = approval();
        without_preview["params"]
            .as_object_mut()
            .unwrap()
            .remove("preview");
        assert!(checked(&without_preview, &builtin("approval")).is_ok());
    }

    #[test]
    fn a_reply_and_a_plan_with_steps_pass() {
        let reply = json!({
            "tpl": "agent.reply@1",
            "params": {
                "text": "Done.",
                "steps": [
                    {"label": "Build", "status": "done"},
                    {"label": "Test", "status": "failed", "detail": "2 failures\nsee log"}
                ]
            },
            "fallback": "Done."
        });
        let body = checked(&reply, &builtin("reply")).unwrap();
        assert_eq!(
            body.params.get("steps"),
            Some(&ParamValue::List(vec![
                ListItem::Step {
                    label: "Build".into(),
                    status: StepStatus::Done,
                    detail: None,
                },
                ListItem::Step {
                    label: "Test".into(),
                    status: StepStatus::Failed,
                    detail: Some("2 failures\nsee log".into()),
                },
            ]))
        );
        let plan = json!({
            "tpl": "agent.plan@1",
            "params": {"title": "Plan", "steps": [], "state": "running"},
            "fallback": "Plan"
        });
        assert!(checked(&plan, &builtin("plan")).is_ok());
    }

    #[test]
    fn a_choose_card_takes_several_options_and_a_plan_approval_offers_two() {
        let choose = builtin("choose");
        let card = json!({
            "tpl": "agent.choose@1",
            "params": {
                "question": "Which checks?\nPick any.",
                "options": [
                    {"id": "lint", "label": "Lint"},
                    {"id": "unit", "label": "Unit tests"},
                    {"id": "e2e", "label": "End to end"}
                ]
            },
            "fallback": "Which checks: lint, unit tests or end to end?"
        });
        let card = checked(&card, &choose).unwrap();
        assert_eq!(card.offered(&choose), ["lint", "unit", "e2e"]);
        assert!(
            answer(CARD, &["lint", "e2e"])
                .unwrap()
                .check(&card, &choose)
                .is_ok()
        );

        let approval = builtin("plan-approval");
        let card = json!({
            "tpl": "agent.plan-approval@1",
            "params": {"title": "Refactor", "plan": "1. Split the module\n2. Add tests"},
            "fallback": "Approve the refactoring plan?"
        });
        let card = checked(&card, &approval).unwrap();
        assert_eq!(card.offered(&approval), ["approve", "reject"]);
    }

    #[test]
    fn refuses_missing_unknown_and_mistyped_params() {
        let definition = builtin("approval");
        let mut body = approval();
        body["params"].as_object_mut().unwrap().remove("details");
        refused(checked(&body, &definition), "params.details: is required");
        let mut body = approval();
        body["params"]["colour"] = json!("red");
        refused(
            checked(&body, &definition),
            "params.colour: is not a param of agent.approval@1",
        );
        let mut body = approval();
        body["params"]["title"] = json!(7);
        refused(
            checked(&body, &definition),
            "params.title: must be a string",
        );
        let mut body = approval();
        body["params"]["title"] = json!(null);
        refused(
            checked(&body, &definition),
            "params.title: must be a string, an integer",
        );
        refused(
            checked(&question(), &definition),
            "tpl: names agent.question@1, but the definition is agent.approval@1",
        );
    }

    #[test]
    fn checks_string_values() {
        let definition = builtin("approval");
        let mut body = approval();
        body["params"]["title"] = json!("a".repeat(120));
        assert!(checked(&body, &definition).is_ok());
        body["params"]["title"] = json!("a".repeat(121));
        refused(
            checked(&body, &definition),
            "params.title: is 121 characters; the limit is 120",
        );
        body["params"]["title"] = json!("two\nlines");
        refused(
            checked(&body, &definition),
            "params.title: contains the control character U+000A",
        );
        body["params"]["title"] = json!("evil\u{202E}txt");
        refused(
            checked(&body, &definition),
            "params.title: contains the invisible",
        );
    }

    #[test]
    fn refuses_empty_strings_when_present() {
        let definition = builtin("approval");
        for text in ["", " ", "\n\t "] {
            let mut body = approval();
            body["params"]["title"] = json!(text);
            refused(checked(&body, &definition), "params.title: is empty");
            let mut body = approval();
            body["params"]["details"] = json!(text);
            refused(checked(&body, &definition), "params.details: is empty");
            let mut body = approval();
            body["params"]["preview"] = json!(text);
            refused(checked(&body, &definition), "params.preview: is empty");
        }
        let definition = custom(
            json!({"tags": {"type": "list", "item": {"type": "string", "max_len": 10}, "max_items": 3}}),
            json!([{"type": "text", "param": "tags"}]),
        );
        assert!(checked(&custom_body(json!({"tags": ["a"]})), &definition).is_ok());
        refused(
            checked(&custom_body(json!({"tags": ["a", " "]})), &definition),
            "params.tags[1]: is empty",
        );
        refused(
            checked(&custom_body(json!({"tags": [""]})), &definition),
            "params.tags[0]: is empty",
        );
    }

    #[test]
    fn checks_enum_int_and_list_values() {
        let plan = builtin("plan");
        let body = json!({"tpl": "agent.plan@1", "params": {"title": "P", "steps": [], "state": "paused"}, "fallback": "P"});
        refused(
            checked(&body, &plan),
            "params.state: \"paused\" is not one of draft, running, done, failed",
        );

        let definition = custom(
            json!({"n": {"type": "int", "min": -2, "max": 3}}),
            json!([{"type": "text", "param": "n"}]),
        );
        assert!(checked(&custom_body(json!({"n": 3})), &definition).is_ok());
        assert!(checked(&custom_body(json!({"n": -2})), &definition).is_ok());
        refused(
            checked(&custom_body(json!({"n": 4})), &definition),
            "params.n: must be -2 to 3, not 4",
        );
        refused(
            checked(&custom_body(json!({"n": 1.5})), &definition),
            "params.n: must be a 64-bit integer",
        );
        refused(
            checked(&custom_body(json!({"n": "1"})), &definition),
            "params.n: must be an integer",
        );

        let steps = |n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| json!({"label": format!("S{i}"), "status": "pending"}))
                .collect()
        };
        let mut body = json!({"tpl": "agent.plan@1", "params": {"title": "P", "steps": steps(50)}, "fallback": "P"});
        assert!(checked(&body, &plan).is_ok());
        body["params"]["steps"] = json!(steps(51));
        refused(
            checked(&body, &plan),
            "params.steps: has 51 items; the limit is 50",
        );
        body["params"]["steps"] = json!([{"label": "S", "status": "paused"}]);
        refused(checked(&body, &plan), "params.steps[0].status");
        body["params"]["steps"] = json!(["plain"]);
        refused(checked(&body, &plan), "params.steps[0]: must be a step");
        body["params"]["steps"] = json!([{"label": "S", "status": "done", "extra": 1}]);
        refused(
            checked(&body, &plan),
            "params.steps[0]: unknown field \"extra\"",
        );
    }

    #[test]
    fn checks_options() {
        let definition = builtin("question");
        let mut body = question();
        body["params"]["options"][1]["id"] = json!("eu_west");
        refused(
            checked(&body, &definition),
            "params.options[1].id: duplicate option id \"eu_west\"",
        );
        for id in ["-us", "eu-west", "1us", "Us", "_us", ""] {
            body["params"]["options"][1]["id"] = json!(id);
            refused(checked(&body, &definition), "params.options[1].id: ");
        }
        body["params"]["options"][1]["id"] = json!("u");
        assert!(checked(&body, &definition).is_ok());
        body["params"]["options"][1] = json!({"id": "us", "label": " "});
        refused(
            checked(&body, &definition),
            "params.options[1].label: is empty",
        );
        body["params"]["options"] = json!([]);
        refused(
            checked(&body, &definition),
            "params.options: offers 0 options",
        );
    }

    #[test]
    fn checks_the_envelope() {
        let mut body = approval();
        body["fallback"] = json!("");
        refused(rich(&body), "fallback: is empty");
        body["fallback"] = json!("a".repeat(2000));
        assert!(rich(&body).is_ok());
        body["fallback"] = json!("a".repeat(2001));
        refused(rich(&body), "fallback: is 2001 characters");
        let mut body = approval();
        body["extra"] = json!(true);
        refused(rich(&body), "unknown field \"extra\"");
        let mut body = approval();
        body["tpl"] = json!("agent.approval@01");
        refused(rich(&body), "tpl: ");
        let mut body = approval();
        body["params"] = json!([]);
        refused(rich(&body), "params: must be an object");
        refused(
            RichBody::parse(
                br#"{"tpl":"agent.approval@1","params":{"title":"a","title":"b"},"fallback":"x"}"#,
            ),
            "duplicate key",
        );
        let mut body = approval();
        body["params"]["details"] = json!("");
        let len = serde_json::to_vec(&body).unwrap().len();
        body["params"]["details"] = json!("a".repeat(MAX_RICH_BYTES + 1 - len));
        refused(rich(&body), "the RICH body is 32769 bytes");
        body["params"]["details"] = json!("a".repeat(MAX_RICH_BYTES - len));
        refused(
            rich(&body).unwrap().check(&builtin("approval")),
            "params.details: is",
        );
    }

    #[test]
    fn answers_to_buttons() {
        let definition = builtin("approval");
        let card = checked(&approval(), &definition).unwrap();
        let ok = answer(CARD, &["approve"]).unwrap();
        assert_eq!(ok.card.to_string(), CARD);
        assert!(ok.check(&card, &definition).is_ok());
        refused(
            answer(CARD, &["maybe"]).unwrap().check(&card, &definition),
            "values[0]: \"maybe\" is not an option of the card",
        );
        refused(
            answer(CARD, &["approve", "reject"])
                .unwrap()
                .check(&card, &definition),
            "values: the card takes exactly 1 value, not 2",
        );
        let reply = builtin("reply");
        let reply_card =
            rich(&json!({"tpl": "agent.reply@1", "params": {"text": "Hi"}, "fallback": "Hi"}))
                .unwrap();
        refused(
            ok.check(&reply_card, &reply),
            "agent.reply@1 has no interactive block",
        );
    }

    #[test]
    fn answers_to_a_question_come_from_the_card() {
        let definition = builtin("question");
        let card = checked(&question(), &definition).unwrap();
        assert_eq!(card.offered(&definition), ["eu_west", "us_1"]);
        assert!(
            answer(CARD, &["us_1"])
                .unwrap()
                .check(&card, &definition)
                .is_ok()
        );
        refused(
            answer(CARD, &["approve"])
                .unwrap()
                .check(&card, &definition),
            "\"approve\" is not an option of the card",
        );
    }

    #[test]
    fn answers_to_multi_respect_min_and_max() {
        let definition = custom(
            json!({}),
            json!([{"type": "multi", "options": [
                {"id": "a", "label": "A"}, {"id": "b", "label": "B"}, {"id": "c", "label": "C"}
            ], "min": 2, "max": 3}]),
        );
        let card = checked(&custom_body(json!({})), &definition).unwrap();
        assert!(
            answer(CARD, &["a", "c"])
                .unwrap()
                .check(&card, &definition)
                .is_ok()
        );
        assert!(
            answer(CARD, &["a", "b", "c"])
                .unwrap()
                .check(&card, &definition)
                .is_ok()
        );
        refused(
            answer(CARD, &["a"]).unwrap().check(&card, &definition),
            "values: the card takes 2 to 3 values, not 1",
        );
    }

    #[test]
    fn checks_the_answer_envelope() {
        refused(
            answer("not-a-uuid", &["a"]),
            "card: \"not-a-uuid\" is not a hyphenated UUID",
        );
        refused(answer("0190f5d27c3a7b4e9a1f2c3d4e5f6a7b", &["a"]), "card:");
        refused(answer(CARD, &[]), "values: needs 1 to 10 values, not 0");
        refused(
            answer(CARD, &["a", "a"]),
            "values[1]: \"a\" is listed twice",
        );
        let eleven: Vec<String> = (0..11).map(|i| format!("v{i}")).collect();
        let eleven: Vec<&str> = eleven.iter().map(String::as_str).collect();
        refused(answer(CARD, &eleven), "not 11");
        for value in ["x\u{0}", "A", "", "eu-west", "1a", "a".repeat(33).as_str()] {
            refused(answer(CARD, &["a", value]), "values[1]: ");
        }
        assert!(answer(CARD, &["a_1", &"a".repeat(32)]).is_ok());
        let body = |fallback: &str| {
            AnswerBody::parse(
                &serde_json::to_vec(&json!({"card": CARD, "values": ["a"], "fallback": fallback}))
                    .unwrap(),
            )
        };
        refused(body(" "), "fallback: is empty");
        refused(body("a\nb"), "fallback: contains the control character");
        assert!(body(&"a".repeat(500)).is_ok());
        refused(body(&"a".repeat(501)), "fallback: is 501 characters");
        refused(
            AnswerBody::parse(br#"{"card":"x","values":["a"],"fallback":"x","extra":1}"#),
            "unknown field \"extra\"",
        );
        refused(
            AnswerBody::parse(&vec![b' '; MAX_ANSWER_BYTES + 1]),
            "the ANSWER body is 4097 bytes",
        );
    }
}
