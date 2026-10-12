use crate::validation::ValidationError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("invalid input: {0}")]
    Invalid(#[from] ValidationError),
    #[error("unknown user")]
    UnknownUser,
    #[error("unknown or revoked device")]
    UnknownDevice,
    #[error("conversation not found")]
    ConversationNotFound,
    #[error("not a member of this conversation")]
    NotMember,
    #[error("the users are not accepted contacts")]
    NotContact,
    #[error("there is no pending contact request")]
    NoPendingRequest,
    #[error("the contact is blocked")]
    Blocked,
    #[error("this client message id was already used by another sender")]
    ClientIdConflict,
    #[error("the sequence number is outside the conversation")]
    SeqOutOfRange,
    #[error("a concurrent change conflicted; retry the operation")]
    Conflict,
    #[error("storage error: {0}")]
    Store(String),
}

impl DomainError {
    /// Whether the same operation may succeed if the caller tries it again unchanged.
    /// Only `Conflict` is retryable: every other error repeats until something changes.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Conflict)
    }
}

pub type DomainResult<T> = Result<T, DomainError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_conflict_is_retryable() {
        assert!(DomainError::Conflict.is_retryable());
        assert!(!DomainError::NotMember.is_retryable());
        assert!(!DomainError::Store("connection lost".into()).is_retryable());
    }
}
