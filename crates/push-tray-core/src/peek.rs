//! The tray's side of the chat peek: the agent's last line for the menu
//! ([`menu_line`]), the messenger's address for "Open in messenger" ([`messenger_url`]),
//! and the Chat tab's view model ([`chat_entries`]).

use push_bridge_control::{CardState, PeekEntry, PeekFrom, PeekKind};
use serde::Serialize;
use serde_json::Value;

/// The longest menu line, in characters (the "…" included).
pub const MENU_LINE_MAX: usize = 60;

/// The agent's newest entry (of any kind), its first non-empty line, trimmed and cut to
/// [`MENU_LINE_MAX`] characters with "…"; `None` without one. `entries` are oldest first.
pub fn menu_line(entries: &[PeekEntry]) -> Option<String> {
    let entry = entries
        .iter()
        .rev()
        .find(|entry| entry.from == PeekFrom::Agent)?;
    let line = entry
        .text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if line.chars().count() <= MENU_LINE_MAX {
        return Some(line.to_owned());
    }
    let cut: String = line.chars().take(MENU_LINE_MAX - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// The messenger's web address from the bridge's `config.json` text: its `api_url`'s
/// host (and port) as `https://<host>/`. Only `https://` URLs with a plain host name or
/// IPv4 address are taken: no user info, no IPv6 brackets, nothing outside
/// `[A-Za-z0-9.-]` plus an optional numeric port.
pub fn messenger_url(config_json: &str) -> Option<String> {
    let config: Value = serde_json::from_str(config_json).ok()?;
    let url = config.get("api_url")?.as_str()?;
    let scheme = url.get(..8)?;
    if !scheme.eq_ignore_ascii_case("https://") {
        return None;
    }
    let rest = &url[8..];
    let authority = rest.find(['/', '?', '#']).map_or(rest, |end| &rest[..end]);
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    let port_ok = port.is_none_or(|port| {
        !port.is_empty() && port.len() <= 5 && port.chars().all(|c| c.is_ascii_digit())
    });
    (host_ok && port_ok).then(|| format!("https://{authority}/"))
}

/// A Chat tab entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatEntry {
    /// Unix milliseconds.
    pub time: i64,
    pub from: PeekFrom,
    pub kind: PeekKind,
    pub text: String,
    pub card_state: Option<CardState>,
}

/// The Chat tab's entries, oldest first (newest last), as the bridge sent them.
pub fn chat_entries(entries: Vec<PeekEntry>) -> Vec<ChatEntry> {
    entries
        .into_iter()
        .map(|entry| ChatEntry {
            time: entry.time,
            from: entry.from,
            kind: entry.kind,
            text: entry.text,
            card_state: entry.card_state,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(from: PeekFrom, kind: PeekKind, text: &str) -> PeekEntry {
        PeekEntry {
            time: 1,
            from,
            kind,
            text: text.into(),
            card_state: None,
        }
    }

    #[test]
    fn the_menu_line_is_the_agents_newest_first_line() {
        let entries = [
            entry(PeekFrom::Agent, PeekKind::Reply, "old"),
            entry(
                PeekFrom::Agent,
                PeekKind::Step,
                "\n  Running the tests  \nmore",
            ),
            entry(PeekFrom::Owner, PeekKind::Text, "from the owner"),
        ];
        assert_eq!(menu_line(&entries).as_deref(), Some("Running the tests"));
        assert_eq!(
            menu_line(&[entry(PeekFrom::Owner, PeekKind::Text, "hi")]),
            None
        );
        assert_eq!(menu_line(&[]), None);
        assert_eq!(
            menu_line(&[entry(PeekFrom::Agent, PeekKind::Card, " \n ")]),
            None
        );
    }

    #[test]
    fn a_long_menu_line_is_cut_with_an_ellipsis() {
        let exact = "é".repeat(MENU_LINE_MAX);
        assert_eq!(
            menu_line(&[entry(PeekFrom::Agent, PeekKind::Reply, &exact)]),
            Some(exact.clone())
        );
        let long = "x".repeat(MENU_LINE_MAX + 1);
        let line = menu_line(&[entry(PeekFrom::Agent, PeekKind::Reply, &long)]).unwrap();
        assert_eq!(line.chars().count(), MENU_LINE_MAX);
        assert!(line.ends_with("x…"));
    }

    #[test]
    fn the_messenger_url_is_an_https_host_only() {
        let url =
            |api_url: &str| messenger_url(&serde_json::json!({ "api_url": api_url }).to_string());
        assert_eq!(
            url("https://push.example.com/api/v1?x=1").as_deref(),
            Some("https://push.example.com/")
        );
        assert_eq!(
            url("https://localhost:8443").as_deref(),
            Some("https://localhost:8443/")
        );
        assert_eq!(
            url("HTTPS://10.0.0.1#frag").as_deref(),
            Some("https://10.0.0.1/")
        );
        for refused in [
            "http://push.example.com/",
            "javascript:alert(1)",
            "https://user:pass@push.example.com/",
            "https://evil.com@push.example.com/",
            "https:///path",
            "https://",
            "https://[::1]:8443/",
            "https://host:/",
            "https://host:abc/",
            "https://ho st/",
            "https://host\\evil/",
            "https://host:1:2/",
            "ftp://host/",
        ] {
            assert_eq!(url(refused), None, "{refused}");
        }
        assert_eq!(messenger_url("{not json"), None);
        assert_eq!(messenger_url(r#"{"api_url": 5}"#), None);
        assert_eq!(messenger_url("{}"), None);
    }

    #[test]
    fn chat_entries_are_copied_in_order_as_camel_case() {
        let mut card = entry(PeekFrom::Agent, PeekKind::Card, "Deploy?");
        card.card_state = Some(CardState::Open);
        card.time = 2;
        let entries = chat_entries(vec![entry(PeekFrom::Owner, PeekKind::Text, "hi"), card]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hi");
        let json = serde_json::to_value(&entries).unwrap();
        assert_eq!(
            json[1],
            serde_json::json!({
                "time": 2,
                "from": "agent",
                "kind": "card",
                "text": "Deploy?",
                "cardState": "open"
            })
        );
        assert_eq!(json[0]["cardState"], Value::Null);
    }
}
