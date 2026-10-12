//! Keys for transaction-scoped advisory locks (`pg_advisory_xact_lock(bigint)`).
//!
//! A key is the 64-bit FNV-1a hash of a fixed tag followed by the 16-byte ids it covers.
//! FNV-1a is fixed by its definition, so every replica and every release computes the same
//! key. The tag namespaces each kind of lock, so a key shared by two kinds is as unlikely
//! as any other 64-bit collision. Any shared key, within a kind or across kinds, only
//! makes two unrelated operations wait for each other; it never lets two operations on one
//! subject run together.

use push_domain::UserId;
use uuid::Uuid;

const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(tag: &[u8], ids: &[Uuid]) -> i64 {
    let bytes = tag
        .iter()
        .chain(ids.iter().flat_map(|id| id.as_bytes().iter()));
    let hash = bytes.fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    });
    // The same 64 bits, read as the signed key Postgres takes.
    hash as i64
}

/// The lock serialising contact operations between two users, the same in either order.
pub(crate) fn contact_pair(a: UserId, b: UserId) -> i64 {
    let (low, high) = if a <= b { (a, b) } else { (b, a) };
    fnv1a(b"contact-pair", &[low.0, high.0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_the_published_test_vectors() {
        assert_eq!(fnv1a(b"", &[]) as u64, 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a", &[]) as u64, 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar", &[]) as u64, 0x8594_4171_f739_67e8);
    }

    #[test]
    fn a_pair_has_one_key_in_either_order() {
        let (a, b) = (UserId::new(), UserId::new());
        assert_eq!(contact_pair(a, b), contact_pair(b, a));
        assert_ne!(contact_pair(a, b), contact_pair(a, UserId::new()));
    }
}
