//! Agent-key signers for push-sdk (docs/agent-key-sign-in.md): [`cng::CngKey`] keeps a
//! non-exportable P-256 key in Windows key storage (the TPM when there is one);
//! [`software::SoftwareKey`] keeps it in a file, for tests and development only.

pub use push_sdk::{AgentClaims, AgentPublicKey, AgentSigner, SignerError};

/// A signer that can also tell its public key and where the key lives.
pub trait AgentKey: AgentSigner {
    /// The key's public half.
    fn public_key(&self) -> AgentPublicKey;
    /// Where the private key is, for people: "Microsoft Platform Crypto Provider (TPM)",
    /// "software file …".
    fn storage(&self) -> String;
}

#[cfg(windows)]
pub mod cng;
pub mod software;
