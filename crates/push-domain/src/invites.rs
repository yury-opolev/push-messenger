//! Invite links and QR codes (spec §5.5).
//!
//! A code is ten characters of Crockford base32: 50 random bits. That is plenty because
//! redeeming is rate limited per account and per IP at the transport layer (spec §5.6):
//! guessing one live code among a million would take about a billion attempts.

use chrono::TimeDelta;
use uuid::Uuid;

use crate::clock::Clock;
use crate::contacts::{are_contacts, contact_changed, set_contact};
use crate::error::{DomainError, DomainResult};
use crate::ids::UserId;
use crate::model::{ContactState, Invite, InviteRecord};
use crate::notify::Notifier;
use crate::service::Messaging;
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::{ValidationError, validate_invite};

/// Crockford's base32 alphabet: digits and letters without I, L, O and U.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in an invite code.
const CODE_LEN: usize = 10;

impl<S, C, N> Messaging<S, C, N>
where
    S: Store,
    C: Clock,
    N: Notifier,
{
    /// Creates an invite that `user` can share as a link or QR code. It lasts `expires_in`
    /// and can be redeemed `max_uses` times.
    ///
    /// A new code that collides with an existing one (about one chance in 10^15 per stored
    /// invite) fails with the retryable `Conflict`.
    pub async fn create_invite(
        &self,
        user: UserId,
        expires_in: TimeDelta,
        max_uses: u32,
    ) -> DomainResult<Invite> {
        validate_invite(expires_in, max_uses)?;
        let mut tx = self.store.begin().await?;
        if !tx.user_exists(user).await? {
            return Err(DomainError::UnknownUser);
        }
        let record = InviteRecord {
            code: invite_code(Uuid::new_v4()),
            user_id: user,
            expires_at: self.clock.now() + expires_in,
            max_uses,
            uses: 0,
            revoked_at: None,
        };
        let invite = Invite::from(&record);
        if tx.insert_invite(record).await? == InsertOutcome::Duplicate {
            return Err(DomainError::Conflict);
        }
        tx.commit().await?;
        Ok(invite)
    }

    /// Revokes one of `user`'s invites. Revoking again does nothing. Someone else's code,
    /// or an unknown one, is `InviteNotFound`.
    pub async fn revoke_invite(&self, user: UserId, code: &str) -> DomainResult<()> {
        let code = normalize_code(code).ok_or(DomainError::InviteNotFound)?;
        let mut tx = self.store.begin().await?;
        let mut invite = tx
            .lock_invite(&code)
            .await?
            .filter(|invite| invite.user_id == user)
            .ok_or(DomainError::InviteNotFound)?;
        if invite.revoked_at.is_none() {
            invite.revoked_at = Some(self.clock.now());
            tx.update_invite(invite).await?;
            tx.commit().await?;
        }
        Ok(())
    }

    /// Makes `redeemer` and the creator of the invite accepted contacts (spec §5.5):
    /// sharing the code is the consent. Returns the creator.
    ///
    /// An unknown, revoked, expired or used-up code is `InviteNotFound`, and so is a code
    /// whose creator has blocked the redeemer: redeeming is not an oracle and never
    /// reveals a block. A redeemer who has blocked the creator gets `Blocked`. Users who
    /// are already contacts succeed without using up the invite. Any pending request
    /// between the two is replaced by the accepted contact.
    pub async fn redeem_invite(&self, redeemer: UserId, code: &str) -> DomainResult<UserId> {
        let code = normalize_code(code).ok_or(DomainError::InviteNotFound)?;
        let now = self.clock.now();
        let mut tx = self.store.begin().await?;
        let mut invite = tx
            .lock_invite(&code)
            .await?
            .filter(|invite| invite.is_usable(now))
            .ok_or(DomainError::InviteNotFound)?;
        let creator = invite.user_id;
        if creator == redeemer {
            return Err(ValidationError::SelfTarget.into());
        }
        if !tx.user_exists(redeemer).await? {
            return Err(DomainError::UnknownUser);
        }
        tx.lock_contact_pair(creator, redeemer).await?;
        if tx.get_contact(creator, redeemer).await? == Some(ContactState::Blocked) {
            return Err(DomainError::InviteNotFound);
        }
        if tx.get_contact(redeemer, creator).await? == Some(ContactState::Blocked) {
            return Err(DomainError::Blocked);
        }
        if are_contacts(&mut tx, creator, redeemer).await? {
            tx.rollback().await?;
            return Ok(creator);
        }

        let accepted = Some(ContactState::Accepted);
        let mut events = Vec::new();
        for (owner, peer) in [(creator, redeemer), (redeemer, creator)] {
            if set_contact(&mut tx, owner, peer, accepted).await? {
                events.push(contact_changed(owner, peer, accepted));
            }
        }
        invite.uses += 1;
        tx.update_invite(invite).await?;
        tx.commit().await?;
        self.publish(events).await;
        Ok(creator)
    }
}

/// The invite code made from the 50 lowest bits of `random`. A version 4 UUID has 62
/// random bits there.
fn invite_code(random: Uuid) -> String {
    let (_, bits) = random.as_u64_pair();
    (0..CODE_LEN)
        .map(|index| char::from(CODE_ALPHABET[((bits >> (5 * index)) & 31) as usize]))
        .collect()
}

/// A code as typed by a person, in canonical form: upper case, without `-` or spaces, with
/// Crockford's look-alikes read as digits (`I` and `L` as `1`, `O` as `0`). `None` if it
/// cannot be a code.
fn normalize_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| match c.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        })
        .collect();
    let valid = code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b));
    valid.then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_takes_five_bits_per_character_from_the_lowest_bits() {
        let counting = (0..10u128).fold(0, |bits, index| bits | (index << (5 * index)));
        assert_eq!(invite_code(Uuid::from_u128(counting)), "0123456789");
        assert_eq!(invite_code(Uuid::from_u128(0)), "0000000000");
        assert_eq!(invite_code(Uuid::from_u128(u128::MAX)), "ZZZZZZZZZZ");
    }

    #[test]
    fn new_codes_use_only_the_alphabet() {
        for _ in 0..100 {
            let code = invite_code(Uuid::new_v4());
            assert_eq!(normalize_code(&code), Some(code));
        }
    }

    #[test]
    fn typed_codes_are_normalized() {
        assert_eq!(normalize_code("abcd-efgh jk"), Some("ABCDEFGHJK".into()));
        assert_eq!(normalize_code("o1il0OIL23"), Some("0111001123".into()));
    }

    #[test]
    fn impossible_codes_are_refused() {
        assert_eq!(normalize_code("ABCDEFGHJ"), None);
        assert_eq!(normalize_code("ABCDEFGHJKM"), None);
        assert_eq!(normalize_code("ABCDEFGHJU"), None);
        assert_eq!(normalize_code("ABCDEFGHJ!"), None);
    }
}
