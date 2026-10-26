//! tpl/1 message templates: definitions, the meta-rules they must pass, and RICH and ANSWER
//! bodies checked against them (spec 2026-10-26, rich message templates).
//!
//! Pure code without I/O, shared by message-svc (enforcement) and the SDK (checks before
//! queueing).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

mod body;
mod builtin;
mod definition;
mod outline;
mod strict;
mod wording;

pub use body::{AnswerBody, ListItem, ParamValue, Params, RichBody, StepStatus};
pub use builtin::builtins;
pub use definition::{
    Block, ButtonOption, ButtonStyle, ChoiceOption, Choices, Definition, ItemType, ParamSpec,
    ParamType, Source,
};
pub use outline::outline;
pub use wording::{check_literal, check_value, looks_like_url};

/// The format tag every definition carries.
pub const FORMAT: &str = "tpl/1";
/// Largest definition, in bytes.
pub const MAX_DEFINITION_BYTES: usize = 16 * 1024;
/// Largest RICH body, in bytes.
pub const MAX_RICH_BYTES: usize = 32 * 1024;
/// Largest ANSWER body, in bytes.
pub const MAX_ANSWER_BYTES: usize = 4 * 1024;
/// Publisher ids no account may claim.
pub const RESERVED_PUBLISHERS: &[&str] = &["agent", "push", "system", "admin", "builtin"];
/// The publisher of the built-in templates, owned by the server.
pub const BUILTIN_PUBLISHER: &str = "agent";

/// Who sent a message, as the server derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SenderKind {
    Human,
    Agent,
}

impl SenderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for SenderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A template version: `<publisher>.<name>@<version>`. Ids match `^[a-z][a-z0-9-]{0,31}$`;
/// the version is 1 to 65535 in decimal, without a sign or leading zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TemplateRef {
    pub publisher: String,
    pub name: String,
    pub version: u16,
}

impl fmt::Display for TemplateRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}@{}", self.publisher, self.name, self.version)
    }
}

impl FromStr for TemplateRef {
    type Err = TemplateError;

    fn from_str(s: &str) -> Result<Self, TemplateError> {
        let bad = || {
            TemplateError(format!(
                "{s:?} is not a template ref (<publisher>.<name>@<version>)"
            ))
        };
        let (id, version) = s.split_once('@').ok_or_else(bad)?;
        let (publisher, name) = id.split_once('.').ok_or_else(bad)?;
        let decimal = !version.is_empty()
            && version.bytes().all(|b| b.is_ascii_digit())
            && !version.starts_with('0');
        if !is_id(publisher) || !is_id(name) || !decimal {
            return Err(bad());
        }
        let version = version.parse().map_err(|_| bad())?;
        Ok(Self {
            publisher: publisher.to_owned(),
            name: name.to_owned(),
            version,
        })
    }
}

/// Why a definition or body was refused: the rule that failed and where.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TemplateError(pub String);

/// `^[a-z][a-z0-9-]{0,31}$`: publisher and template names.
pub(crate) fn is_id(s: &str) -> bool {
    matches_pattern(
        s,
        |b| b.is_ascii_lowercase(),
        |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-',
    )
}

/// `^[a-z][a-z0-9_]{0,31}$`: param names, enum values and option ids (literal, in bodies
/// and in answers).
pub(crate) fn is_key(s: &str) -> bool {
    matches_pattern(
        s,
        |b| b.is_ascii_lowercase(),
        |b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_',
    )
}

/// 1 to 32 bytes: a `first` byte, then `rest` bytes.
pub(crate) fn matches_pattern(s: &str, first: fn(u8) -> bool, rest: fn(u8) -> bool) -> bool {
    let bytes = s.as_bytes();
    match bytes.split_first() {
        Some((head, tail)) => bytes.len() <= 32 && first(*head) && tail.iter().all(|b| rest(*b)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_round_trips() {
        for text in ["agent.approval@1", "a.b-2@65535", "acme-co.deploy-check@42"] {
            let parsed: TemplateRef = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
        }
        assert_eq!(
            "agent.reply@7".parse::<TemplateRef>(),
            Ok(TemplateRef {
                publisher: "agent".into(),
                name: "reply".into(),
                version: 7,
            })
        );
    }

    #[test]
    fn malformed_refs_are_refused() {
        let long = format!("{}.b@1", "a".repeat(33));
        for text in [
            "A.b@1",
            "a.b@0",
            "a.b@65536",
            "a.b@01",
            "a@1",
            "a.b.c@1",
            "a.b@+1",
            "a.b@",
            "a.b",
            ".b@1",
            "1a.b@1",
            "a_b.c@1",
            "a.b@1@2",
            long.as_str(),
        ] {
            assert!(text.parse::<TemplateRef>().is_err(), "{text}");
        }
    }

    #[test]
    fn sender_kinds_display_in_lowercase() {
        assert_eq!(SenderKind::Human.to_string(), "human");
        assert_eq!(SenderKind::Agent.to_string(), "agent");
    }
}
