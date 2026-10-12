//! Input rules shared by the server and, later, the SDK.

/// Largest text message body, in bytes.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Largest history page.
pub const MAX_HISTORY_PAGE: u32 = 100;
/// Largest batch returned by one sync.
pub const MAX_SYNC_BATCH: u32 = 500;
/// Unread counts stop here; clients show "99+".
pub const MAX_UNREAD: u32 = 100;

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
}
