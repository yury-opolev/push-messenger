//! A P-256 key in memory or in a PKCS#8 file.
//!
//! **For tests and development only.** The private key is a plain file anyone with the
//! user's rights can copy; a real agent keeps its key in OS key storage
//! ([`crate::cng::CngKey`] on Windows).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};

use crate::{AgentClaims, AgentKey, AgentPublicKey, AgentSigner, SignerError};

/// A software P-256 signing key. **For tests and development only**: the private key is
/// not protected by the OS.
pub struct SoftwareKey {
    key_pair: EcdsaKeyPair,
    rng: SystemRandom,
    public_key: AgentPublicKey,
    path: Option<PathBuf>,
}

impl SoftwareKey {
    /// A new random key, in memory only.
    pub fn generate() -> Result<Self, SignerError> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| error("generate", "generating the key failed"))?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    /// The key in a PKCS#8 (v1 or v2) P-256 document.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, SignerError> {
        let rng = SystemRandom::new();
        let key_pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, &rng)
            .map_err(|_| error("bad_key", "the PKCS#8 document is not a P-256 key"))?;
        let public_key = public_key_of(key_pair.public_key().as_ref())?;
        Ok(Self {
            key_pair,
            rng,
            public_key,
            path: None,
        })
    }

    /// The key in the PKCS#8 file at `path`, or a new one written there (with the parent
    /// folders, and the default permissions of the user's folder).
    pub fn load_or_create(path: &Path) -> Result<Self, SignerError> {
        let mut key = match fs::read(path) {
            Ok(pkcs8) => Self::from_pkcs8(&pkcs8)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let rng = SystemRandom::new();
                let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| error("generate", "generating the key failed"))?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        error("io", &format!("creating the key folder failed: {e}"))
                    })?;
                }
                fs::write(path, pkcs8.as_ref())
                    .map_err(|e| error("io", &format!("writing the key file failed: {e}")))?;
                Self::from_pkcs8(pkcs8.as_ref())?
            }
            Err(e) => return Err(error("io", &format!("reading the key file failed: {e}"))),
        };
        key.path = Some(path.to_owned());
        Ok(key)
    }
}

fn public_key_of(uncompressed: &[u8]) -> Result<AgentPublicKey, SignerError> {
    let bad = || {
        error(
            "bad_key",
            "the public key is not an uncompressed P-256 point",
        )
    };
    if uncompressed.len() != 65 || uncompressed[0] != 0x04 {
        return Err(bad());
    }
    Ok(AgentPublicKey {
        x: uncompressed[1..33].try_into().map_err(|_| bad())?,
        y: uncompressed[33..65].try_into().map_err(|_| bad())?,
    })
}

fn error(code: &str, message: &str) -> SignerError {
    SignerError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

impl AgentSigner for SoftwareKey {
    fn sign(&self, claims: &AgentClaims) -> Result<String, SignerError> {
        push_sdk::assemble_assertion(&self.public_key, claims, |input| {
            let signature = self
                .key_pair
                .sign(&self.rng, input)
                .map_err(|_| error("sign", "signing failed"))?;
            signature
                .as_ref()
                .try_into()
                .map_err(|_| error("sign", "the signature is not 64 bytes"))
        })
    }
}

impl AgentKey for SoftwareKey {
    fn public_key(&self) -> AgentPublicKey {
        self.public_key
    }

    fn storage(&self) -> String {
        match &self.path {
            Some(path) => format!("software file {}", path.display()),
            None => "software key in memory".to_owned(),
        }
    }
}

impl fmt::Debug for SoftwareKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SoftwareKey")
            .field("storage", &self.storage())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use push_auth_core::agent_key::verify_assertion;

    const AUDIENCE: &str = "https://push.test";
    const NOW: i64 = 1_800_000_000;

    fn jti_of(token: &str) -> String {
        let payload = token.split('.').nth(1).expect("payload");
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url"))
                .expect("JSON");
        claims["jti"].as_str().expect("jti").to_owned()
    }

    #[test]
    fn the_signature_passes_the_servers_verifier() {
        let key = SoftwareKey::generate().unwrap();
        let token = key.sign(&AgentClaims::new(AUDIENCE, NOW)).unwrap();
        assert_eq!(
            verify_assertion(&token, AUDIENCE, NOW),
            Ok(key.public_key().thumbprint())
        );
    }

    #[test]
    fn the_server_refuses_another_audience() {
        let key = SoftwareKey::generate().unwrap();
        let token = key.sign(&AgentClaims::new(AUDIENCE, NOW)).unwrap();
        assert!(verify_assertion(&token, "https://other.test", NOW).is_err());
    }

    #[test]
    fn fresh_claims_have_fresh_jtis() {
        let key = SoftwareKey::generate().unwrap();
        let first = key.sign(&AgentClaims::new(AUDIENCE, NOW)).unwrap();
        let second = key.sign(&AgentClaims::new(AUDIENCE, NOW)).unwrap();
        assert_ne!(jti_of(&first), jti_of(&second));
    }

    #[test]
    fn load_or_create_keeps_the_key() {
        let dir = std::env::temp_dir().join(format!("push-agent-key-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("agent.pk8");
        let first = SoftwareKey::load_or_create(&path);
        let second = SoftwareKey::load_or_create(&path);
        let _ = fs::remove_dir_all(&dir);
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first.public_key(), second.public_key());
        assert!(second.storage().starts_with("software file "));
    }

    #[test]
    fn a_bad_pkcs8_document_is_refused() {
        let err = SoftwareKey::from_pkcs8(b"not a key").unwrap_err();
        assert_eq!(err.code, "bad_key");
    }
}
