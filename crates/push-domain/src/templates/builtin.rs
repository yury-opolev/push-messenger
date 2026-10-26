//! The built-in `agent.*` templates, seeded as approved and owned by the server.

use super::Definition;

const SOURCES: [&str; 4] = [
    include_str!("builtin/agent.reply.json"),
    include_str!("builtin/agent.approval.json"),
    include_str!("builtin/agent.question.json"),
    include_str!("builtin/agent.plan.json"),
];

/// `agent.reply@1`, `agent.approval@1`, `agent.question@1` and `agent.plan@1`.
pub fn builtins() -> Vec<Definition> {
    SOURCES
        .iter()
        .map(|source| Definition::parse(source.as_bytes()).expect("a built-in template is valid"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{BUILTIN_PUBLISHER, SenderKind};
    use super::*;

    #[test]
    fn every_builtin_parses_and_round_trips() {
        let refs: Vec<String> = builtins()
            .iter()
            .map(|d| d.template_ref().to_string())
            .collect();
        assert_eq!(
            refs,
            [
                "agent.reply@1",
                "agent.approval@1",
                "agent.question@1",
                "agent.plan@1"
            ]
        );
        for definition in builtins() {
            assert_eq!(definition.template_ref().publisher, BUILTIN_PUBLISHER);
            assert_eq!(definition.senders(), [SenderKind::Agent]);
            let again = Definition::parse(definition.canonical_json().as_bytes()).unwrap();
            assert_eq!(again, definition);
        }
    }

    #[test]
    fn builtins_have_their_titles_and_interactive_blocks() {
        let titles: Vec<(String, bool)> = builtins()
            .iter()
            .map(|d| (d.title().to_owned(), d.interactive().is_some()))
            .collect();
        assert_eq!(
            titles,
            [
                ("Agent reply".to_owned(), false),
                ("Approval request".to_owned(), true),
                ("Question".to_owned(), true),
                ("Plan".to_owned(), false),
            ]
        );
    }
}
