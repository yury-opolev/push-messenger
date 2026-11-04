//! The protocol version, `push-voice-adapter/<n>`, and its negotiation: the bridge offers
//! the versions it speaks in its hello, the adapter answers with one of them, the highest
//! both speak.

use std::fmt;

/// This crate's protocol version.
pub const PROTOCOL: &str = "push-voice-adapter/1";
/// Every version this crate speaks, newest first.
pub const SUPPORTED: &[&str] = &[PROTOCOL];

const PREFIX: &str = "push-voice-adapter/";

/// A parsed `push-voice-adapter/<n>` (n ≥ 1, decimal, no leading zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(pub u32);

impl ProtocolVersion {
    pub const V1: ProtocolVersion = ProtocolVersion(1);

    pub fn parse(text: &str) -> Option<Self> {
        let number = text.strip_prefix(PREFIX)?;
        if number.is_empty()
            || number.len() > 9
            || number.starts_with('0')
            || !number.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        number.parse().ok().map(ProtocolVersion)
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PREFIX}{}", self.0)
    }
}

/// The highest version in both `offered` and `supported`, as `supported` spells it;
/// `None` when they share none. Strings that are not versions are ignored.
pub fn negotiate<'a, I>(offered: I, supported: &[&str]) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let offered: Vec<ProtocolVersion> = offered
        .into_iter()
        .filter_map(ProtocolVersion::parse)
        .collect();
    supported
        .iter()
        .filter_map(|text| ProtocolVersion::parse(text))
        .filter(|version| offered.contains(version))
        .max()
        .map(|version| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_strictly() {
        assert_eq!(ProtocolVersion::parse(PROTOCOL), Some(ProtocolVersion::V1));
        assert_eq!(
            ProtocolVersion::parse("push-voice-adapter/12"),
            Some(ProtocolVersion(12))
        );
        for bad in [
            "",
            "push-voice-adapter/",
            "push-voice-adapter/0",
            "push-voice-adapter/01",
            "push-voice-adapter/1.0",
            "push-voice-adapter/+1",
            "push-voice-adapter/1 ",
            "push-voice-adapter/9999999999",
            "PUSH-VOICE-ADAPTER/1",
            "other/1",
        ] {
            assert_eq!(ProtocolVersion::parse(bad), None, "{bad:?}");
        }
        assert_eq!(ProtocolVersion(3).to_string(), "push-voice-adapter/3");
    }

    #[test]
    fn the_highest_common_version_wins() {
        let both = ["push-voice-adapter/1", "push-voice-adapter/2"];
        assert_eq!(
            negotiate(["push-voice-adapter/2", "push-voice-adapter/1"], &both),
            Some("push-voice-adapter/2".to_owned())
        );
        assert_eq!(
            negotiate(["push-voice-adapter/1", "junk"], &both),
            Some("push-voice-adapter/1".to_owned())
        );
        assert_eq!(negotiate(["push-voice-adapter/3"], SUPPORTED), None);
        assert_eq!(negotiate([], SUPPORTED), None);
        assert_eq!(negotiate([PROTOCOL], SUPPORTED), Some(PROTOCOL.to_owned()));
    }
}
