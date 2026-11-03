//! Who may call whom (spec D11, D12).

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::ContactState;
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{Store, Tx};
use crate::validation::ValidationError;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Whether `caller` may call `callee`: accepted contacts both ways with no block
    /// either way, or an owned agent and its owner with no block (even without contact
    /// rows). An agent calls only its owner, and only its owner calls it. Every refusal is
    /// `NotContact`, so the answer never reveals which ids exist, a block or an agent;
    /// `Invalid(SelfTarget)` for oneself. Reads only, publishes nothing.
    pub async fn call_permission(&self, caller: UserId, callee: UserId) -> DomainResult<()> {
        if caller == callee {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        let result = permission(&mut tx, caller, callee).await;
        tx.rollback().await?;
        result
    }
}

async fn permission<T: Tx>(tx: &mut T, caller: UserId, callee: UserId) -> DomainResult<()> {
    if !tx.user_exists(caller).await? || !tx.user_exists(callee).await? {
        return Err(DomainError::NotContact);
    }
    let blocked = Some(ContactState::Blocked);
    if tx.get_contact(caller, callee).await? == blocked
        || tx.get_contact(callee, caller).await? == blocked
    {
        return Err(DomainError::NotContact);
    }
    if let Some(owner) = tx.agent_owner(caller).await? {
        return allowed(owner == callee);
    }
    if let Some(owner) = tx.agent_owner(callee).await? {
        return allowed(owner == caller);
    }
    allowed(are_contacts(tx, caller, callee).await?)
}

fn allowed(yes: bool) -> DomainResult<()> {
    if yes {
        Ok(())
    } else {
        Err(DomainError::NotContact)
    }
}
