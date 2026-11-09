//! Who may call whom (spec D11, D12; spec 2026-11-09 proactive agents D11).

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::{ContactState, OutgoingAction};
use crate::notify::Notifier;
use crate::outgoing::{Ask, Gate, acting_agent, gate, use_call_grant};
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
    /// rows). Only its owner calls an agent. An agent calls its owner, and a person who is
    /// its accepted contact under its owner's call policy (spec 2026-11-09 proactive
    /// agents D11): an approved call is used first (once, within [`CALL_GRANT`]), else
    /// `off` is `NotAllowedByOwner`, `ask` stores a waiting action (`WaitingForOwner`) and
    /// `allowed` counts the call (`RateLimited`). Two agents that are accepted contacts get
    /// `NotAllowed` (agents talk in messages and cards only); every other refusal is
    /// `NotContact`, so the answer never reveals which ids exist, a block or an agent;
    /// `Invalid(SelfTarget)` for oneself. Returns whether the caller is an agent (the
    /// call is started by an agent).
    ///
    /// [`CALL_GRANT`]: crate::outgoing::CALL_GRANT
    pub async fn call_permission(&self, caller: UserId, callee: UserId) -> DomainResult<bool> {
        if caller == callee {
            return Err(ValidationError::SelfTarget.into());
        }
        let mut tx = self.store.begin().await?;
        match permission(&mut tx, caller, callee, self.clock.now()).await {
            Ok(Permit::Allowed { by_agent }) => {
                tx.commit().await?;
                Ok(by_agent)
            }
            Ok(Permit::Waiting { owner, action, new }) => {
                self.wait_for_owner(tx, owner, action, new).await
            }
            Err(error) => {
                tx.rollback().await?;
                Err(error)
            }
        }
    }
}

/// What [`permission`] allows.
enum Permit {
    Allowed {
        by_agent: bool,
    },
    /// The agent's call waits for its owner (stored on the transaction when `new`).
    Waiting {
        owner: UserId,
        action: OutgoingAction,
        new: bool,
    },
}

async fn permission<T: Tx>(
    tx: &mut T,
    caller: UserId,
    callee: UserId,
    now: DateTime<Utc>,
) -> DomainResult<Permit> {
    if !tx.user_exists(caller).await? || !tx.user_exists(callee).await? {
        return Err(DomainError::NotContact);
    }
    let blocked = Some(ContactState::Blocked);
    if tx.get_contact(caller, callee).await? == blocked
        || tx.get_contact(callee, caller).await? == blocked
    {
        return Err(DomainError::NotContact);
    }
    let caller_owner = tx.agent_owner(caller).await?;
    let callee_owner = tx.agent_owner(callee).await?;
    if caller_owner.is_some() && callee_owner.is_some() {
        // Agents talk to each other in messages and cards only (spec 2026-11-06 agent
        // connections D8). Agents that are not contacts stay strangers.
        return if are_contacts(tx, caller, callee).await? {
            Err(DomainError::NotAllowed)
        } else {
            Err(DomainError::NotContact)
        };
    }
    if let Some(owner) = caller_owner {
        if owner == callee {
            return Ok(Permit::Allowed { by_agent: true });
        }
        if !are_contacts(tx, caller, callee).await? || acting_agent(tx, caller).await?.is_none() {
            return Err(DomainError::NotContact);
        }
        // Under the agent's lock, so one approved call rings once.
        tx.lock_users(&[caller]).await?;
        if use_call_grant(tx, caller, callee, now).await? {
            return Ok(Permit::Allowed { by_agent: true });
        }
        return match gate(tx, caller, Ask::call(callee), now).await? {
            Gate::Proceed => Ok(Permit::Allowed { by_agent: true }),
            Gate::Waiting { action, new } => Ok(Permit::Waiting { owner, action, new }),
        };
    }
    if let Some(owner) = callee_owner {
        return allowed(owner == caller);
    }
    allowed(are_contacts(tx, caller, callee).await?)
}

fn allowed(yes: bool) -> DomainResult<Permit> {
    if yes {
        Ok(Permit::Allowed { by_agent: false })
    } else {
        Err(DomainError::NotContact)
    }
}
