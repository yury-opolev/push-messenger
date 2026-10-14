//! What the API accepts: emails, handles, display names, passwords, devices and user ids.
//!
//! Each function trims and normalises its input and returns what is stored, or the
//! reason it is refused.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An input the API refuses: which field and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid {
    pub field: &'static str,
    pub reason: &'static str,
}

fn invalid(field: &'static str, reason: &'static str) -> Invalid {
    Invalid { field, reason }
}

/// Most characters of a password.
pub const MAX_PASSWORD_CHARS: usize = 128;
/// Fewest characters of a password.
pub const MIN_PASSWORD_CHARS: usize = 8;
/// Most characters of a display name or device name.
pub const MAX_NAME_CHARS: usize = 64;
/// Most user ids one `GET /v1/users?ids=…` may ask for.
pub const MAX_PROFILE_IDS: usize = 100;

/// An email address: trimmed, with the domain in lower case. Matching ignores case
/// anyway (the column is citext); the local part keeps the case the user typed.
pub fn email(raw: &str) -> Result<String, Invalid> {
    let raw = raw.trim();
    let refuse = || invalid("email", "not a valid email address");
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(refuse());
    }
    let (local, domain) = raw.split_once('@').ok_or_else(refuse)?;
    let labels_ok = domain
        .split('.')
        .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'));
    if local.is_empty() || local.len() > 64 || domain.contains('@') || !domain.contains('.') {
        return Err(refuse());
    }
    if !labels_ok {
        return Err(refuse());
    }
    // Lower-casing can change the byte count, and the database limits the stored bytes.
    let normalised = format!("{local}@{}", domain.to_lowercase());
    if normalised.len() > 254 {
        return Err(refuse());
    }
    Ok(normalised)
}

/// A public handle, without its `@`: 3 to 30 lower-case letters, digits and underscores,
/// starting with a letter. Input may carry the `@` and any case.
pub fn handle(raw: &str) -> Result<String, Invalid> {
    let raw = raw.trim();
    let handle = raw.strip_prefix('@').unwrap_or(raw).to_lowercase();
    let mut chars = handle.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !(3..=30).contains(&handle.len()) || !starts_with_letter || !rest_ok {
        return Err(invalid(
            "handle",
            "use 3 to 30 letters, digits or underscores, starting with a letter",
        ));
    }
    Ok(handle)
}

/// A display name: trimmed, 1 to 64 characters, no control characters.
pub fn display_name(raw: &str) -> Result<String, Invalid> {
    name(raw, "display_name")
}

/// A device's name, like a display name.
pub fn device_name(raw: &str) -> Result<String, Invalid> {
    name(raw, "name")
}

fn name(raw: &str, field: &'static str) -> Result<String, Invalid> {
    let name = raw.trim();
    let count = name.chars().count();
    if count == 0 || count > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(invalid(
            field,
            "use 1 to 64 characters without control characters",
        ));
    }
    Ok(name.to_owned())
}

/// A new password: 8 to 128 characters. Used as typed: never trimmed.
pub fn password(raw: &str) -> Result<(), Invalid> {
    let count = raw.chars().count();
    if !(MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&count) {
        return Err(invalid("password", "use 8 to 128 characters"));
    }
    Ok(())
}

/// The ids of `GET /v1/users?ids=…`: 1 to 100 UUIDs, separated by commas, sorted and
/// without duplicates.
pub fn user_ids(raw: &str) -> Result<Vec<Uuid>, Invalid> {
    let mut ids = raw
        .split(',')
        .map(|id| Uuid::parse_str(id.trim()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("ids", "use user ids separated by commas"))?;
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_PROFILE_IDS {
        return Err(invalid("ids", "ask for at most 100 users at once"));
    }
    Ok(ids)
}

/// What kind of device a client runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Ios,
    Android,
    Web,
    Desktop,
    Other,
}

impl Platform {
    /// The stored name, as the `device.platform` CHECK lists it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Other => "other",
        }
    }

    /// The platform with this stored name.
    pub fn parse(name: &str) -> Result<Self, Invalid> {
        Ok(match name {
            "ios" => Self::Ios,
            "android" => Self::Android,
            "web" => Self::Web,
            "desktop" => Self::Desktop,
            "other" => Self::Other,
            _ => {
                return Err(invalid(
                    "platform",
                    "use ios, android, web, desktop or other",
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_ids_are_a_comma_separated_list_of_at_most_100() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(user_ids(&format!("{b}, {a},{b}")), Ok(vec![a, b]));
        for bad in ["", "not-a-uuid", &format!("{a},,{b}")] {
            assert_eq!(user_ids(bad).unwrap_err().field, "ids", "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_PROFILE_IDS)
            .map(|_| Uuid::now_v7().to_string())
            .collect();
        assert_eq!(
            user_ids(&many.join(",")).unwrap_err().reason,
            "ask for at most 100 users at once"
        );
        assert_eq!(user_ids(&many[1..].join(",")).map(|ids| ids.len()), Ok(100));
    }

    #[test]
    fn emails_are_trimmed_and_their_domain_lower_cased() {
        assert_eq!(
            email("  Alice.Smith@Example.COM "),
            Ok("Alice.Smith@example.com".to_owned())
        );
        for bad in [
            "",
            "alice",
            "@example.com",
            "alice@",
            "alice@localhost",
            "alice@@example.com",
            "al ice@example.com",
            "alice@example..com",
            "alice@-example.com",
        ] {
            assert_eq!(
                email(bad),
                Err(invalid("email", "not a valid email address")),
                "{bad:?}"
            );
        }
        let long_local = format!("{}@example.com", "a".repeat(65));
        assert!(email(&long_local).is_err());
    }

    #[test]
    fn the_email_limit_counts_bytes_after_lower_casing() {
        // 'İ' is 2 bytes and lower-cases to 3: 254 bytes typed, 378 stored.
        let typed = format!("a@{}.com", "İ".repeat(124));
        assert_eq!(typed.len(), 254);
        assert!(email(&typed).is_err());
        let fits = format!("a@{}.com", "İ".repeat(40));
        assert!(email(&fits).is_ok());
    }

    #[test]
    fn handles_drop_the_at_sign_and_case() {
        assert_eq!(handle("@Alice_99"), Ok("alice_99".to_owned()));
        assert_eq!(handle("bob"), Ok("bob".to_owned()));
        for bad in ["al", "9lives", "has-dash", "_bob", "émile", &"a".repeat(31)] {
            assert!(handle(bad).is_err(), "{bad:?}");
        }
        assert!(handle(&"a".repeat(30)).is_ok());
    }

    #[test]
    fn names_are_trimmed_and_limited_to_64_characters() {
        assert_eq!(display_name("  Alice  "), Ok("Alice".to_owned()));
        assert_eq!(device_name("Yuri's iPhone"), Ok("Yuri's iPhone".to_owned()));
        assert!(display_name(&"é".repeat(64)).is_ok());
        assert!(display_name(&"é".repeat(65)).is_err());
        assert!(display_name("   ").is_err());
        assert!(display_name("tab\there").is_err());
        assert_eq!(device_name("").unwrap_err().field, "name");
    }

    #[test]
    fn passwords_have_8_to_128_characters() {
        assert!(password("1234567").is_err());
        assert!(password("12345678").is_ok());
        assert!(password(&"ü".repeat(128)).is_ok());
        assert!(password(&"ü".repeat(129)).is_err());
    }

    #[test]
    fn platforms_round_trip_through_their_names() {
        for platform in [
            Platform::Ios,
            Platform::Android,
            Platform::Web,
            Platform::Desktop,
            Platform::Other,
        ] {
            assert_eq!(Platform::parse(platform.as_str()), Ok(platform));
        }
        assert!(Platform::parse("toaster").is_err());
        assert!(Platform::parse("IOS").is_err());
    }
}
