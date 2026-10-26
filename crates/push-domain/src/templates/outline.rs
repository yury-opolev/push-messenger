//! A plain-text outline of a definition, the admin's preview.

use std::fmt::Write;

use super::{Block, Choices, Definition, ItemType, ParamType, Source};

/// One header line (`<ref> "<title>" senders: ...`), one line per block, then the params.
pub fn outline(definition: &Definition) -> String {
    let senders: Vec<&str> = definition.senders().iter().map(|s| s.as_str()).collect();
    let mut out = format!(
        "{} {:?} senders: {}\n",
        definition.template_ref(),
        definition.title(),
        senders.join(", ")
    );
    for block in definition.layout() {
        out.push_str(&block_line(definition, block));
        out.push('\n');
    }
    out.push_str("params:");
    if definition.params().is_empty() {
        out.push_str(" none");
    }
    for (name, spec) in definition.params() {
        let _ = write!(out, "\n  {name}: {}", param_type(&spec.ty));
        if spec.optional {
            out.push_str(", optional");
        }
    }
    out
}

fn block_line(definition: &Definition, block: &Block) -> String {
    let source = |param: &str| {
        let optional = definition.param(param).is_some_and(|p| p.optional);
        format!("{{{param}}}{}", if optional { " (optional)" } else { "" })
    };
    let labelled = |label: &Option<String>| match label {
        Some(label) => format!("{} {label:?}", block.kind()),
        None => block.kind().to_owned(),
    };
    match block {
        Block::Heading(text) | Block::Text(text) => {
            let shown = match text {
                Source::Literal(literal) => format!("{literal:?}"),
                Source::Param(param) => source(param),
            };
            format!("{}: {shown}", block.kind())
        }
        Block::Preview { param, label }
        | Block::Thinking { param, label }
        | Block::Steps { param, label }
        | Block::Status { param, label } => format!("{}: {}", labelled(label), source(param)),
        Block::Buttons { options } => {
            let options: Vec<String> = options
                .iter()
                .map(|o| format!("{} {:?} ({})", o.id, o.label, o.style.as_str()))
                .collect();
            format!("buttons: {}", options.join(" | "))
        }
        Block::Single { label, choices } | Block::Multi { label, choices, .. } => {
            let shown = match choices {
                Choices::Literal(options) => options
                    .iter()
                    .map(|o| format!("{} {:?}", o.id, o.label))
                    .collect::<Vec<_>>()
                    .join(" | "),
                Choices::Param(param) => source(param),
            };
            let mut line = format!("{}: {shown}", labelled(label));
            if let Block::Multi { min, max, .. } = block {
                let _ = write!(line, " (choose {min} to {max})");
            }
            line
        }
    }
}

fn param_type(ty: &ParamType) -> String {
    match ty {
        ParamType::String { max_len, multiline } => format!(
            "string, max_len {max_len}{}",
            if *multiline { ", multiline" } else { "" }
        ),
        ParamType::Enum { values } => format!("enum [{}]", values.join(", ")),
        ParamType::Int { min, max } => format!("int, {min} to {max}"),
        ParamType::Bool => "bool".to_owned(),
        ParamType::List { item, max_items } => {
            let item = match item {
                ItemType::String { max_len } => format!("string (max_len {max_len})"),
                ItemType::Option => "option".to_owned(),
                ItemType::Step => "step".to_owned(),
            };
            format!("list of {item}, max_items {max_items}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::builtins;
    use super::*;

    fn builtin(name: &str) -> Definition {
        builtins()
            .into_iter()
            .find(|d| d.template_ref().name == name)
            .unwrap()
    }

    #[test]
    fn outlines_the_approval_template() {
        assert_eq!(
            outline(&builtin("approval")),
            concat!(
                "agent.approval@1 \"Approval request\" senders: agent\n",
                "heading: {title}\n",
                "text: {details}\n",
                "preview \"Preview\": {preview} (optional)\n",
                "buttons: approve \"Approve\" (primary) | reject \"Reject\" (danger)\n",
                "params:\n",
                "  title: string, max_len 120\n",
                "  details: string, max_len 4000, multiline\n",
                "  preview: string, max_len 4000, multiline, optional",
            )
        );
    }

    #[test]
    fn outlines_choices_enums_and_lists() {
        let question = outline(&builtin("question"));
        assert!(
            question.contains("single \"Choose one\": {options}\n"),
            "{question}"
        );
        assert!(
            question.contains("  options: list of option, max_items 10"),
            "{question}"
        );
        let plan = outline(&builtin("plan"));
        assert!(
            plan.contains("status \"State\": {state} (optional)\n"),
            "{plan}"
        );
        assert!(
            plan.contains("  state: enum [draft, running, done, failed], optional"),
            "{plan}"
        );
    }
}
