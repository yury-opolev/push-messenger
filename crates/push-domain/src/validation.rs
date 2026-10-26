//! Input rules shared by the server and, later, the SDK.

use chrono::TimeDelta;

/// Largest text message body, in bytes.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Largest history page.
pub const MAX_HISTORY_PAGE: u32 = 100;
/// Largest batch returned by one sync.
pub const MAX_SYNC_BATCH: u32 = 500;
/// Unread counts stop here; clients show "99+".
pub const MAX_UNREAD: u32 = 100;
/// Most uses one invite allows.
pub const MAX_INVITE_USES: u32 = 100;
/// Shortest invite lifetime, in seconds: one minute.
pub const MIN_INVITE_LIFETIME_SECS: i64 = 60;
/// Longest invite lifetime, in seconds: 30 days.
pub const MAX_INVITE_LIFETIME_SECS: i64 = 30 * 24 * 60 * 60;
/// Longest group title, in characters, after trimming.
pub const MAX_TITLE_CHARS: usize = 100;
/// Most members a group can have, admins included.
pub const MAX_GROUP_MEMBERS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("message body is empty")]
    EmptyBody,
    #[error("message body is {actual} bytes; the limit is {limit}")]
    BodyTooLarge { actual: usize, limit: usize },
    #[error("message body is not valid UTF-8")]
    NotUtf8,
    #[error("a user cannot target themselves")]
    SelfTarget,
    #[error("an invite allows 1 to {limit} uses, not {actual}")]
    InviteUses { actual: u32, limit: u32 },
    #[error("an invite lasts {min} to {max} seconds, not {actual}")]
    InviteLifetime { actual: i64, min: i64, max: i64 },
    #[error("a group title cannot be empty")]
    EmptyTitle,
    #[error("a group title is {actual} characters; the limit is {limit}")]
    TitleTooLong { actual: usize, limit: usize },
    #[error("a group title cannot contain control characters")]
    TitleControl,
    #[error("a catalog position cannot be negative, not {actual}")]
    CatalogPosition { actual: i64 },
    #[error("a reviewer is 1 to 64 printable characters, not only spaces")]
    Reviewer,
    #[error(
        "a review note is at most 500 characters, without control characters other than line breaks"
    )]
    ReviewNote,
    #[error("this content type cannot be sent")]
    ContentType,
    #[error("the activity label {0}")]
    ActivityLabel(String),
    #[error("the activity state is missing")]
    ActivityState,
    #[error("a presence read names 1 to 100 users")]
    PresenceBatch,
}

/// Checks a text message body.
pub fn validate_text(body: &[u8]) -> Result<(), ValidationError> {
    if body.is_empty() {
        return Err(ValidationError::EmptyBody);
    }
    if body.len() > MAX_TEXT_BYTES {
        return Err(ValidationError::BodyTooLarge {
            actual: body.len(),
            limit: MAX_TEXT_BYTES,
        });
    }
    if std::str::from_utf8(body).is_err() {
        return Err(ValidationError::NotUtf8);
    }
    Ok(())
}

/// Checks the limits of a new invite: 1 to `MAX_INVITE_USES` uses, and a lifetime from
/// `MIN_INVITE_LIFETIME_SECS` to `MAX_INVITE_LIFETIME_SECS`, both inclusive.
pub fn validate_invite(expires_in: TimeDelta, max_uses: u32) -> Result<(), ValidationError> {
    if !(1..=MAX_INVITE_USES).contains(&max_uses) {
        return Err(ValidationError::InviteUses {
            actual: max_uses,
            limit: MAX_INVITE_USES,
        });
    }
    if expires_in < TimeDelta::seconds(MIN_INVITE_LIFETIME_SECS)
        || expires_in > TimeDelta::seconds(MAX_INVITE_LIFETIME_SECS)
    {
        return Err(ValidationError::InviteLifetime {
            actual: expires_in.num_seconds(),
            min: MIN_INVITE_LIFETIME_SECS,
            max: MAX_INVITE_LIFETIME_SECS,
        });
    }
    Ok(())
}

/// Checks a group title and returns it without leading and trailing whitespace. It must
/// keep 1 to `MAX_TITLE_CHARS` characters and no control characters (PostgreSQL text
/// refuses NUL, and line breaks have no place in a title).
pub fn validate_title(title: &str) -> Result<String, ValidationError> {
    let title = title.trim();
    if title.chars().any(char::is_control) {
        return Err(ValidationError::TitleControl);
    }
    let chars = title.chars().count();
    if chars == 0 {
        return Err(ValidationError::EmptyTitle);
    }
    if chars > MAX_TITLE_CHARS {
        return Err(ValidationError::TitleTooLong {
            actual: chars,
            limit: MAX_TITLE_CHARS,
        });
    }
    Ok(title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_text() {
        assert_eq!(validate_text("hello".as_bytes()), Ok(()));
    }

    #[test]
    fn rejects_an_empty_body() {
        assert_eq!(validate_text(b""), Err(ValidationError::EmptyBody));
    }

    #[test]
    fn accepts_a_body_exactly_at_the_limit() {
        assert_eq!(validate_text(&vec![b'a'; MAX_TEXT_BYTES]), Ok(()));
    }

    #[test]
    fn rejects_a_body_one_byte_over_the_limit() {
        assert_eq!(
            validate_text(&vec![b'a'; MAX_TEXT_BYTES + 1]),
            Err(ValidationError::BodyTooLarge {
                actual: MAX_TEXT_BYTES + 1,
                limit: MAX_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn rejects_invalid_utf8() {
        assert_eq!(validate_text(&[0xff, 0xfe]), Err(ValidationError::NotUtf8));
    }

    #[test]
    fn invite_limits_are_inclusive() {
        let shortest = TimeDelta::seconds(MIN_INVITE_LIFETIME_SECS);
        let longest = TimeDelta::seconds(MAX_INVITE_LIFETIME_SECS);
        assert_eq!(validate_invite(shortest, 1), Ok(()));
        assert_eq!(validate_invite(longest, MAX_INVITE_USES), Ok(()));
    }

    #[test]
    fn rejects_invites_outside_the_limits() {
        let day = TimeDelta::days(1);
        for uses in [0, MAX_INVITE_USES + 1] {
            assert_eq!(
                validate_invite(day, uses),
                Err(ValidationError::InviteUses {
                    actual: uses,
                    limit: MAX_INVITE_USES,
                })
            );
        }
        for seconds in [MIN_INVITE_LIFETIME_SECS - 1, MAX_INVITE_LIFETIME_SECS + 1] {
            assert_eq!(
                validate_invite(TimeDelta::seconds(seconds), 1),
                Err(ValidationError::InviteLifetime {
                    actual: seconds,
                    min: MIN_INVITE_LIFETIME_SECS,
                    max: MAX_INVITE_LIFETIME_SECS,
                })
            );
        }
    }

    #[test]
    fn a_title_is_trimmed_and_may_have_exactly_the_limit() {
        assert_eq!(validate_title("  Trip  "), Ok("Trip".to_string()));
        let longest = "é".repeat(MAX_TITLE_CHARS);
        assert_eq!(validate_title(&longest), Ok(longest.clone()));
    }

    #[test]
    fn rejects_empty_and_long_titles() {
        assert_eq!(validate_title(" \t "), Err(ValidationError::EmptyTitle));
        assert_eq!(
            validate_title(&"a".repeat(MAX_TITLE_CHARS + 1)),
            Err(ValidationError::TitleTooLong {
                actual: MAX_TITLE_CHARS + 1,
                limit: MAX_TITLE_CHARS,
            })
        );
    }

    #[test]
    fn rejects_control_characters_inside_titles() {
        for title in ["a\u{0}b", "line\nbreak", "tab\there", "del\u{7f}"] {
            assert_eq!(
                validate_title(title),
                Err(ValidationError::TitleControl),
                "{title:?}"
            );
        }
        assert_eq!(validate_title("  Ærø team  "), Ok("Ærø team".to_owned()));
    }
}
