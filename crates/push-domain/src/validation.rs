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
}
