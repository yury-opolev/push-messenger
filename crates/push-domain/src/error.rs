use uuid::Uuid;

use crate::templates::TemplateError;
use crate::validation::ValidationError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("invalid input: {0}")]
    Invalid(#[from] ValidationError),
    #[error("unknown user")]
    UnknownUser,
    #[error("unknown or revoked device")]
    UnknownDevice,
    /// message-svc has not heard of the device yet: auth-svc registered it, but the worker
    /// has not relayed it (that takes about a second). Unlike `UnknownDevice`, it passes.
    #[error("the device is not known yet; retry shortly")]
    DeviceNotKnownYet,
    /// The access token was issued no later than the user's last password reset that
    /// message-svc heard of: the device must sign in again.
    #[error("the password was changed after this sign-in; sign in again")]
    PasswordChanged,
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
    #[error("this client message id was already used for a different entry")]
    ClientIdConflict,
    #[error("the sequence number is outside the conversation")]
    SeqOutOfRange,
    #[error("the target message does not exist in this conversation")]
    TargetNotFound,
    #[error("only the sender of a message can change it")]
    NotSender,
    #[error("the message is already deleted")]
    AlreadyDeleted,
    #[error("the time allowed for this change has passed")]
    WindowExpired,
    /// The invite does not exist, was revoked, has expired or is used up. One error for
    /// all of these, so redeeming is not an oracle.
    #[error("invite not found")]
    InviteNotFound,
    #[error("a group has at most 100 members")]
    GroupFull,
    #[error("this is not a group")]
    NotAGroup,
    #[error("only an admin of this group can do that")]
    NotAdmin,
    #[error("a group must keep at least one admin")]
    LastAdmin,
    /// A template definition or a RICH or ANSWER body broke a tpl/1 rule.
    #[error("invalid template: {0}")]
    Template(#[from] TemplateError),
    /// The card already has its answer; later answers are refused.
    #[error("the card is already answered")]
    CardAnswered,
    #[error("template not found")]
    TemplateNotFound,
    /// A submitted version is already stored (its definition never changes).
    #[error("this template version already exists")]
    TemplateExists,
    /// A submission or review the registry's state does not allow: the version's status
    /// does not allow the review, the publisher belongs to another account, or the account
    /// has too many pending submissions.
    #[error("{0}")]
    TemplateConflict(&'static str),
    /// The rules of agents forbid it (spec 2026-11-06 agent connections): a call between
    /// two agents, an invite created by an agent, an agent blocking its owner, the owner as
    /// the target of an owner action.
    #[error("agents may not do this")]
    NotAllowed,
    /// An agent tried to accept, decline or block a request that waits for its owner.
    #[error("only the agent's owner decides on contact requests")]
    OwnerDecides,
    /// An agent's limits on contact requests (spec 2026-11-06 agent connections §4) and
    /// on its other actions (spec 2026-11-09 proactive agents §5).
    #[error("too many requests; retry in {retry_after_s} s")]
    RateLimited { retry_after_s: u32 },
    /// Not an agent of this owner (unknown, another owner's, not an agent, or deleted):
    /// one answer for all, so it reveals nothing.
    #[error("agent not found")]
    AgentNotFound,
    /// The action waits for the agent's owner (spec 2026-11-09 proactive agents, D4): the
    /// owner's outgoing policy for it is `ask`. Asked again while it waits, the same id.
    #[error("the action waits for the owner's approval")]
    WaitingForOwner { action_id: Uuid },
    /// The owner's outgoing policy for the action is `off` (D3).
    #[error("the owner does not allow this action")]
    NotAllowedByOwner,
    /// No waiting action of this agent with this id: unknown, another agent's, or
    /// already decided.
    #[error("outgoing action not found")]
    OutgoingNotFound,
    #[error("a concurrent change conflicted; retry the operation")]
    Conflict,
    /// A revocation or password reset was stored, but its kick could not be delivered to
    /// the gateways: the caller (the worker's relay) retries, which kicks again.
    #[error("the kick could not be delivered; retry: {0}")]
    KickFailed(String),
    #[error("storage error: {0}")]
    Store(String),
}

impl DomainError {
    /// Whether the same operation may succeed if the caller tries it again unchanged.
    /// `Conflict`, `DeviceNotKnownYet` and `KickFailed` are retryable: every other error
    /// repeats until something changes.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Conflict | Self::DeviceNotKnownYet | Self::KickFailed(_)
        )
    }
}

pub type DomainResult<T> = Result<T, DomainError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_conflict_a_device_not_known_yet_and_a_failed_kick_are_retryable() {
        assert!(DomainError::Conflict.is_retryable());
        assert!(DomainError::DeviceNotKnownYet.is_retryable());
        assert!(DomainError::KickFailed("redis down".into()).is_retryable());
        assert!(!DomainError::UnknownDevice.is_retryable());
        assert!(!DomainError::NotMember.is_retryable());
        assert!(!DomainError::Store("connection lost".into()).is_retryable());
    }
}
