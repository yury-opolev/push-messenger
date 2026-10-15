//! The key of the local database (owner decision): an app may open the client with a key,
//! and then the whole SQLite file — every page of the history, the outbox, the contacts and
//! the write-ahead log — is encrypted with it (SQLite3 Multiple Ciphers, its default
//! ChaCha20-Poly1305 scheme). Without the key nobody can read the user's messages from the
//! file, not even the app itself.
//!
//! The SDK never stores the key: the app provides it every time it opens the client, for
//! example by deriving it from a PIN or a password the user enters ([`DatabaseKey::derive`]).
//! A lost key cannot be recovered: the data must be erased and loaded again from the
//! server.

use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use zeroize::{Zeroize, Zeroizing};

use crate::SdkError;

/// A database key: 32 bytes. Wiped from memory when dropped; `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct DatabaseKey {
    bytes: [u8; DatabaseKey::LEN],
}

impl DatabaseKey {
    /// The length of a key, in bytes.
    pub const LEN: usize = 32;
    /// The length of a salt [`DatabaseKey::new_salt`] makes, in bytes; [`DatabaseKey::derive`]
    /// accepts any salt of at least this length.
    pub const SALT_LEN: usize = 16;

    /// A key from 32 bytes the app holds (from a hardware keystore, say).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SdkError> {
        let bytes: [u8; Self::LEN] = bytes
            .try_into()
            .map_err(|_| SdkError::Invalid(format!("a database key is {} bytes", Self::LEN)))?;
        Ok(Self { bytes })
    }

    /// Derives a key from what the user knows — a PIN, a password — with Argon2id
    /// (19 MiB, 2 passes, 1 lane: OWASP's recommendation, as auth-svc uses for passwords).
    /// `salt` is random, made once per database with [`DatabaseKey::new_salt`] and kept by
    /// the app next to it (it is not secret); the same secret and salt always give the same
    /// key. Takes a few tens of milliseconds: call it off the UI thread.
    pub fn derive(secret: &[u8], salt: &[u8]) -> Result<Self, SdkError> {
        if salt.len() < Self::SALT_LEN {
            return Err(SdkError::Invalid(format!(
                "a salt has at least {} bytes",
                Self::SALT_LEN
            )));
        }
        let params = Params::new(19 * 1024, 2, 1, Some(Self::LEN))
            .map_err(|error| SdkError::Invalid(error.to_string()))?;
        let mut bytes = [0; Self::LEN];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(secret, salt, &mut bytes)
            .map_err(|error| SdkError::Invalid(error.to_string()))?;
        Ok(Self { bytes })
    }

    /// The key's 32 bytes, for an app that keeps a derived key itself (in a hardware
    /// keystore, say) and for the bindings, which hand it to the app. Never log them.
    pub fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.bytes
    }

    /// A new random salt for [`DatabaseKey::derive`].
    pub fn new_salt() -> [u8; Self::SALT_LEN] {
        let mut salt = [0; Self::SALT_LEN];
        rand::rng().fill_bytes(&mut salt);
        salt
    }

    /// The key as SQLite3 Multiple Ciphers' `hexkey` and `hexrekey` take it.
    pub(crate) fn hex(&self) -> Zeroizing<String> {
        let mut hex = Zeroizing::new(String::with_capacity(Self::LEN * 2));
        for byte in self.bytes {
            hex.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
            hex.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
        }
        hex
    }
}

impl Drop for DatabaseKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl std::fmt::Debug for DatabaseKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DatabaseKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_derived_the_same_way_every_time_and_never_shown() {
        let salt = [7; DatabaseKey::SALT_LEN];
        let key = DatabaseKey::derive(b"1234", &salt).unwrap();
        assert_eq!(key, DatabaseKey::derive(b"1234", &salt).unwrap());
        assert_ne!(key, DatabaseKey::derive(b"1235", &salt).unwrap());
        assert_ne!(
            key,
            DatabaseKey::derive(b"1234", &DatabaseKey::new_salt()).unwrap()
        );
        assert_eq!(key.hex().len(), 64);
        assert_eq!(format!("{key:?}"), "DatabaseKey(<redacted>)");
        assert!(DatabaseKey::derive(b"1234", &[0; 8]).is_err());
        assert!(DatabaseKey::from_bytes(&[1; 31]).is_err());
        assert_eq!(DatabaseKey::from_bytes(key.as_bytes()).unwrap(), key);
        assert_eq!(
            DatabaseKey::from_bytes(&[0xab; 32]).unwrap().hex().as_str(),
            "ab".repeat(32)
        );
    }
}
