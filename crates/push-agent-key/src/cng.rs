//! A P-256 key in Windows key storage (CNG/NCrypt): the TPM through the Microsoft Platform
//! Crypto Provider when there is one, otherwise the Microsoft Software Key Storage
//! Provider. The key is persisted under a name in the user's key store and is not
//! exportable; signing happens inside the provider.

#![allow(unsafe_code)]

use std::fmt;
use std::mem;
use std::ptr;

use windows_sys::Win32::Foundation::{
    NTE_BAD_ALGID, NTE_BAD_DATA, NTE_BAD_FLAGS, NTE_BAD_KEY_STATE, NTE_BAD_KEYSET, NTE_BAD_TYPE,
    NTE_BUFFER_TOO_SMALL, NTE_DEVICE_NOT_FOUND, NTE_DEVICE_NOT_READY, NTE_EXISTS,
    NTE_INVALID_HANDLE, NTE_INVALID_PARAMETER, NTE_NO_KEY, NTE_NO_MEMORY, NTE_NOT_FOUND,
    NTE_NOT_SUPPORTED, NTE_PERM, NTE_PROVIDER_DLL_FAIL, NTE_SILENT_CONTEXT, NTE_USER_CANCELLED,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALGORITHM, BCRYPT_ECDSA_PUBLIC_P256_MAGIC,
    MS_KEY_STORAGE_PROVIDER, MS_PLATFORM_CRYPTO_PROVIDER, NCRYPT_EXPORT_POLICY_PROPERTY,
    NCRYPT_HANDLE, NCRYPT_PERSIST_FLAG, NCRYPT_SILENT_FLAG, NCryptCreatePersistedKey,
    NCryptDeleteKey, NCryptExportKey, NCryptFinalizeKey, NCryptFreeObject, NCryptOpenKey,
    NCryptOpenStorageProvider, NCryptSetProperty, NCryptSignHash,
};
use windows_sys::core::{HRESULT, PCWSTR};

use crate::{AgentClaims, AgentKey, AgentPublicKey, AgentSigner, SignerError};

/// A Windows key storage provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// The Microsoft Platform Crypto Provider: the key lives in the TPM.
    Platform,
    /// The Microsoft Software Key Storage Provider: the key is encrypted with the user's
    /// DPAPI keys.
    Software,
}

impl Provider {
    /// The providers [`CngKey::open_or_create`] tries, best first.
    pub const ALL: [Provider; 2] = [Provider::Platform, Provider::Software];

    /// The provider's name, for people.
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Platform => "Microsoft Platform Crypto Provider (TPM)",
            Provider::Software => "Microsoft Software Key Storage Provider",
        }
    }

    fn ncrypt_name(self) -> PCWSTR {
        match self {
            Provider::Platform => MS_PLATFORM_CRYPTO_PROVIDER,
            Provider::Software => MS_KEY_STORAGE_PROVIDER,
        }
    }
}

/// Where an NCrypt operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// NCryptOpenStorageProvider: the provider itself is not there.
    OpenProvider,
    /// Anything after the provider was opened.
    Key,
}

/// What a failed open says about the key in that provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenOutcome {
    /// The provider works and does not have the key.
    Missing,
    /// The provider is not usable on this machine.
    Unavailable,
    /// Anything else: the key may well be there, so do not create a new one.
    Failed,
}

/// What to do after a failed create in a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateNext {
    /// Someone else created the key meanwhile: open theirs.
    Reopen,
    /// The provider cannot hold the key: try the next one.
    NextProvider,
    /// Return the error; the caller may retry later.
    Stop,
}

fn classify_open(provider: Provider, step: Step, status: Option<HRESULT>) -> OpenOutcome {
    if step == Step::OpenProvider {
        return OpenOutcome::Unavailable;
    }
    match status {
        Some(NTE_BAD_KEYSET | NTE_NOT_FOUND | NTE_NO_KEY) => OpenOutcome::Missing,
        Some(status) if platform_unavailable(provider, status) => OpenOutcome::Unavailable,
        _ => OpenOutcome::Failed,
    }
}

fn after_create_failure(provider: Provider, step: Step, status: Option<HRESULT>) -> CreateNext {
    if status == Some(NTE_EXISTS) {
        return CreateNext::Reopen;
    }
    // Software is the last resort; a busy, locked-out or refusing TPM is retried later
    // rather than replaced by a software key.
    if provider != Provider::Platform {
        return CreateNext::Stop;
    }
    match (step, status) {
        (Step::OpenProvider, _) => CreateNext::NextProvider,
        (Step::Key, Some(NTE_BAD_ALGID)) => CreateNext::NextProvider,
        (Step::Key, Some(status)) if platform_unavailable(provider, status) => {
            CreateNext::NextProvider
        }
        _ => CreateNext::Stop,
    }
}

/// No TPM, or a TPM the Platform Crypto Provider cannot use.
fn platform_unavailable(provider: Provider, status: HRESULT) -> bool {
    provider == Provider::Platform
        && matches!(
            status,
            NTE_DEVICE_NOT_FOUND | NTE_PROVIDER_DLL_FAIL | NTE_NOT_SUPPORTED
        )
}

/// A failed NCrypt step, keeping the raw HRESULT for [`classify_open`] and
/// [`after_create_failure`].
#[derive(Debug)]
struct Failure {
    step: Step,
    /// None when the failure is not an NCrypt status (a bad name, a bad public key).
    status: Option<HRESULT>,
    error: SignerError,
}

impl From<SignerError> for Failure {
    fn from(error: SignerError) -> Self {
        Self {
            step: Step::Key,
            status: None,
            error,
        }
    }
}

impl From<Failure> for SignerError {
    fn from(failure: Failure) -> Self {
        failure.error
    }
}

/// What looking for a key in every provider found.
enum Lookup {
    Found(CngKey),
    /// No provider has the key; the error is from the last provider asked.
    Absent(SignerError),
    /// A provider failed in a way that does not rule out the key being there.
    Failed(SignerError),
}

/// An NCrypt provider or key handle, freed on drop.
struct Handle(NCRYPT_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: the handle came from NCryptOpenStorageProvider, NCryptOpenKey or
            // NCryptCreatePersistedKey, is owned by this value and is freed only here.
            unsafe { NCryptFreeObject(self.0) };
        }
    }
}

/// A persisted, non-exportable P-256 key in Windows key storage.
pub struct CngKey {
    // Declared before `_provider`: the key handle is freed first.
    key: Handle,
    _provider: Handle,
    provider: Provider,
    name: String,
    public_key: AgentPublicKey,
}

// SAFETY: NCrypt key and provider handles are not bound to the thread that opened them,
// and NCrypt's functions may be called on one handle from several threads. CngKey never
// mutates its handles after construction and frees them only in Drop.
unsafe impl Send for CngKey {}
// SAFETY: as for Send; `&CngKey` only signs and exports the public key.
unsafe impl Sync for CngKey {}

impl CngKey {
    /// The key named `name` in the first provider that has it, or a new one: in the TPM if
    /// the Platform Crypto Provider can create it, otherwise in the software provider.
    ///
    /// Creates only when every provider either lacks the key or is unavailable; any other
    /// failure is returned so that an existing key is never replaced by a new identity.
    pub fn open_or_create(name: &str) -> Result<Self, SignerError> {
        match Self::lookup(name) {
            Lookup::Found(key) => return Ok(key),
            Lookup::Failed(e) => return Err(e),
            Lookup::Absent(_) => {}
        }
        let mut last = None;
        for provider in Provider::ALL {
            let failure = match Self::create_raw(provider, name) {
                Ok(key) => return Ok(key),
                Err(failure) => failure,
            };
            match after_create_failure(provider, failure.step, failure.status) {
                // Another process won the race: use its key, not a second one.
                CreateNext::Reopen => return Self::open_in(provider, name),
                CreateNext::NextProvider => last = Some(failure.error),
                CreateNext::Stop => return Err(failure.error),
            }
        }
        Err(last.expect("Provider::ALL is not empty"))
    }

    /// The key named `name` in the first provider that has it.
    pub fn open(name: &str) -> Result<Self, SignerError> {
        match Self::lookup(name) {
            Lookup::Found(key) => Ok(key),
            Lookup::Absent(e) | Lookup::Failed(e) => Err(e),
        }
    }

    fn lookup(name: &str) -> Lookup {
        let mut absent = None;
        let mut failed = None;
        for provider in Provider::ALL {
            let failure = match Self::open_raw(provider, name) {
                Ok(key) => return Lookup::Found(key),
                Err(failure) => failure,
            };
            match classify_open(provider, failure.step, failure.status) {
                OpenOutcome::Missing | OpenOutcome::Unavailable => absent = Some(failure.error),
                OpenOutcome::Failed => {
                    failed.get_or_insert(failure.error);
                }
            }
        }
        match (failed, absent) {
            (Some(e), _) => Lookup::Failed(e),
            (None, Some(e)) => Lookup::Absent(e),
            (None, None) => unreachable!("Provider::ALL is not empty"),
        }
    }

    /// The key named `name` in `provider`.
    pub fn open_in(provider: Provider, name: &str) -> Result<Self, SignerError> {
        Ok(Self::open_raw(provider, name)?)
    }

    fn open_raw(provider: Provider, name: &str) -> Result<Self, Failure> {
        let wide_name = wide(name)?;
        let prov = open_provider(provider)?;
        let mut key = Handle(0);
        // SAFETY: `prov.0` is an open provider handle, `key.0` is a valid out pointer and
        // `wide_name` is NUL-terminated and outlives the call.
        let status = unsafe {
            NCryptOpenKey(
                prov.0,
                &mut key.0,
                wide_name.as_ptr(),
                0,
                NCRYPT_SILENT_FLAG,
            )
        };
        check(status, "NCryptOpenKey", provider)?;
        Self::from_handles(key, prov, provider, name)
    }

    /// A new non-exportable P-256 key named `name` in `provider`; fails if the name is
    /// taken there.
    pub fn create_in(provider: Provider, name: &str) -> Result<Self, SignerError> {
        Ok(Self::create_raw(provider, name)?)
    }

    fn create_raw(provider: Provider, name: &str) -> Result<Self, Failure> {
        let wide_name = wide(name)?;
        let prov = open_provider(provider)?;
        let mut key = Handle(0);
        // SAFETY: `prov.0` is an open provider handle, `key.0` is a valid out pointer, and
        // the algorithm id and `wide_name` are NUL-terminated and outlive the call.
        let status = unsafe {
            NCryptCreatePersistedKey(
                prov.0,
                &mut key.0,
                BCRYPT_ECDSA_P256_ALGORITHM,
                wide_name.as_ptr(),
                0,
                0,
            )
        };
        check(status, "NCryptCreatePersistedKey", provider)?;
        let policy: u32 = 0;
        // SAFETY: `key.0` is a key handle not yet finalized; the property name is
        // NUL-terminated and the input is the 4 bytes of `policy`, which outlives the call.
        let status = unsafe {
            NCryptSetProperty(
                key.0,
                NCRYPT_EXPORT_POLICY_PROPERTY,
                (&raw const policy).cast(),
                mem::size_of::<u32>() as u32,
                NCRYPT_PERSIST_FLAG | NCRYPT_SILENT_FLAG,
            )
        };
        check(status, "NCryptSetProperty(Export Policy)", provider)?;
        // SAFETY: `key.0` is a key handle not yet finalized.
        let status = unsafe { NCryptFinalizeKey(key.0, NCRYPT_SILENT_FLAG) };
        check(status, "NCryptFinalizeKey", provider)?;
        Self::from_handles(key, prov, provider, name).inspect_err(|_| {
            // The key is persisted but unusable: remove it so the name is free again.
            let _ = Self::delete_in(provider, name);
        })
    }

    /// Deletes the key named `name` from every provider that has it.
    pub fn delete(name: &str) -> Result<(), SignerError> {
        let mut deleted = false;
        let mut last = None;
        for provider in Provider::ALL {
            match Self::delete_in(provider, name) {
                Ok(()) => deleted = true,
                Err(e) => last = Some(e),
            }
        }
        match last {
            Some(e) if !deleted => Err(e),
            _ => Ok(()),
        }
    }

    /// Deletes the key named `name` from `provider`.
    pub fn delete_in(provider: Provider, name: &str) -> Result<(), SignerError> {
        let wide_name = wide(name)?;
        let prov = open_provider(provider)?;
        let mut key = Handle(0);
        // SAFETY: as in `open_in`.
        let status = unsafe {
            NCryptOpenKey(
                prov.0,
                &mut key.0,
                wide_name.as_ptr(),
                0,
                NCRYPT_SILENT_FLAG,
            )
        };
        check(status, "NCryptOpenKey", provider)?;
        // SAFETY: `key.0` is an open key handle. On success NCryptDeleteKey frees it.
        // Flags 0: the Platform Crypto Provider refuses NCRYPT_SILENT_FLAG here.
        let status = unsafe { NCryptDeleteKey(key.0, 0) };
        check(status, "NCryptDeleteKey", provider)?;
        mem::forget(key);
        Ok(())
    }

    /// The provider the key lives in.
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// The key's name in its provider.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn from_handles(
        key: Handle,
        prov: Handle,
        provider: Provider,
        name: &str,
    ) -> Result<Self, Failure> {
        let blob = export(&key, BCRYPT_ECCPUBLIC_BLOB, provider)?;
        let public_key = parse_public_blob(&blob)?;
        Ok(Self {
            key,
            _provider: prov,
            provider,
            name: name.to_owned(),
            public_key,
        })
    }

    #[cfg(test)]
    fn export_blob(&self, blob_type: PCWSTR) -> Result<Vec<u8>, SignerError> {
        Ok(export(&self.key, blob_type, self.provider)?)
    }

    fn sign_hash(&self, hash: &[u8]) -> Result<[u8; 64], SignerError> {
        let mut signature = [0u8; 64];
        let mut written = 0u32;
        // SAFETY: `self.key.0` is an open key handle; null padding info is correct for
        // ECDSA; `hash` and `signature` are valid for the lengths passed; `written` is a
        // valid out pointer.
        let status = unsafe {
            NCryptSignHash(
                self.key.0,
                ptr::null(),
                hash.as_ptr(),
                hash.len() as u32,
                signature.as_mut_ptr(),
                signature.len() as u32,
                &mut written,
                NCRYPT_SILENT_FLAG,
            )
        };
        check(status, "NCryptSignHash", self.provider)?;
        if written != 64 {
            return Err(SignerError {
                code: "bad_signature".to_owned(),
                message: format!("NCryptSignHash returned {written} bytes, not 64"),
            });
        }
        Ok(signature)
    }
}

impl AgentSigner for CngKey {
    fn sign(&self, claims: &AgentClaims) -> Result<String, SignerError> {
        push_sdk::assemble_assertion(&self.public_key, claims, |input| {
            let hash = ring::digest::digest(&ring::digest::SHA256, input);
            self.sign_hash(hash.as_ref())
        })
    }
}

impl AgentKey for CngKey {
    fn public_key(&self) -> AgentPublicKey {
        self.public_key
    }

    fn storage(&self) -> String {
        self.provider.display_name().to_owned()
    }
}

impl fmt::Debug for CngKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CngKey")
            .field("provider", &self.provider)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn open_provider(provider: Provider) -> Result<Handle, Failure> {
    let mut prov = Handle(0);
    // SAFETY: `prov.0` is a valid out pointer; the provider name is a NUL-terminated
    // static string.
    let status = unsafe { NCryptOpenStorageProvider(&mut prov.0, provider.ncrypt_name(), 0) };
    check(status, "NCryptOpenStorageProvider", provider).map_err(|failure| Failure {
        step: Step::OpenProvider,
        ..failure
    })?;
    Ok(prov)
}

fn export(key: &Handle, blob_type: PCWSTR, provider: Provider) -> Result<Vec<u8>, Failure> {
    let mut size = 0u32;
    // SAFETY: `key.0` is an open key handle, `blob_type` a NUL-terminated static string;
    // a null output buffer asks for the size, written to `size`.
    let status = unsafe {
        NCryptExportKey(
            key.0,
            0,
            blob_type,
            ptr::null(),
            ptr::null_mut(),
            0,
            &mut size,
            NCRYPT_SILENT_FLAG,
        )
    };
    check(status, "NCryptExportKey", provider)?;
    let mut blob = vec![0u8; size as usize];
    // SAFETY: as above, with `blob` valid for `size` bytes.
    let status = unsafe {
        NCryptExportKey(
            key.0,
            0,
            blob_type,
            ptr::null(),
            blob.as_mut_ptr(),
            size,
            &mut size,
            NCRYPT_SILENT_FLAG,
        )
    };
    check(status, "NCryptExportKey", provider)?;
    blob.truncate(size as usize);
    Ok(blob)
}

/// `BCRYPT_ECCKEY_BLOB { dwMagic, cbKey = 32 } || X || Y`, little-endian header.
fn parse_public_blob(blob: &[u8]) -> Result<AgentPublicKey, SignerError> {
    let bad = || SignerError {
        code: "bad_public_key".to_owned(),
        message: "the provider's public key is not a P-256 ECDSA key".to_owned(),
    };
    if blob.len() != 8 + 64 {
        return Err(bad());
    }
    let magic = u32::from_le_bytes(blob[0..4].try_into().map_err(|_| bad())?);
    let length = u32::from_le_bytes(blob[4..8].try_into().map_err(|_| bad())?);
    if magic != BCRYPT_ECDSA_PUBLIC_P256_MAGIC || length != 32 {
        return Err(bad());
    }
    Ok(AgentPublicKey {
        x: blob[8..40].try_into().map_err(|_| bad())?,
        y: blob[40..72].try_into().map_err(|_| bad())?,
    })
}

fn wide(name: &str) -> Result<Vec<u16>, SignerError> {
    if name.is_empty() || name.contains('\0') {
        return Err(SignerError {
            code: "bad_key_name".to_owned(),
            message: "the key name is empty or contains NUL".to_owned(),
        });
    }
    Ok(name.encode_utf16().chain(std::iter::once(0)).collect())
}

fn check(status: HRESULT, operation: &str, provider: Provider) -> Result<(), Failure> {
    if status >= 0 {
        return Ok(());
    }
    let hex = format!("0x{:08X}", status as u32);
    let code = match status_name(status) {
        Some(name) => format!("{name} ({hex})"),
        None => hex,
    };
    Err(Failure {
        step: Step::Key,
        status: Some(status),
        error: SignerError {
            code,
            message: format!("{operation} failed in the {}", provider.display_name()),
        },
    })
}

fn status_name(status: HRESULT) -> Option<&'static str> {
    Some(match status {
        NTE_BAD_ALGID => "NTE_BAD_ALGID",
        NTE_BAD_DATA => "NTE_BAD_DATA",
        NTE_BAD_FLAGS => "NTE_BAD_FLAGS",
        NTE_BAD_KEY_STATE => "NTE_BAD_KEY_STATE",
        NTE_BAD_KEYSET => "NTE_BAD_KEYSET",
        NTE_BAD_TYPE => "NTE_BAD_TYPE",
        NTE_BUFFER_TOO_SMALL => "NTE_BUFFER_TOO_SMALL",
        NTE_DEVICE_NOT_FOUND => "NTE_DEVICE_NOT_FOUND",
        NTE_DEVICE_NOT_READY => "NTE_DEVICE_NOT_READY",
        NTE_EXISTS => "NTE_EXISTS",
        NTE_INVALID_HANDLE => "NTE_INVALID_HANDLE",
        NTE_INVALID_PARAMETER => "NTE_INVALID_PARAMETER",
        NTE_NO_KEY => "NTE_NO_KEY",
        NTE_NO_MEMORY => "NTE_NO_MEMORY",
        NTE_NOT_FOUND => "NTE_NOT_FOUND",
        NTE_NOT_SUPPORTED => "NTE_NOT_SUPPORTED",
        NTE_PERM => "NTE_PERM",
        NTE_PROVIDER_DLL_FAIL => "NTE_PROVIDER_DLL_FAIL",
        NTE_SILENT_CONTEXT => "NTE_SILENT_CONTEXT",
        NTE_USER_CANCELLED => "NTE_USER_CANCELLED",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use push_auth_core::agent_key::verify_assertion;
    use windows_sys::Win32::Security::Cryptography::BCRYPT_ECCPRIVATE_BLOB;

    const AUDIENCE: &str = "https://push.test";
    const NOW: i64 = 1_800_000_000;

    /// Deletes the test key even when an assertion fails.
    struct Cleanup(Provider, String);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = CngKey::delete_in(self.0, &self.1);
        }
    }

    fn test_key_name() -> String {
        format!("push-agent-key-test-{}", uuid::Uuid::new_v4())
    }

    fn sign_verify_and_refuse_export(provider: Provider) {
        let name = test_key_name();
        let _cleanup = Cleanup(provider, name.clone());
        let key = CngKey::create_in(provider, &name).unwrap();
        assert_eq!(key.provider(), provider);

        let token = key.sign(&AgentClaims::new(AUDIENCE, NOW)).unwrap();
        assert_eq!(
            verify_assertion(&token, AUDIENCE, NOW),
            Ok(key.public_key().thumbprint())
        );
        assert!(key.export_blob(BCRYPT_ECCPRIVATE_BLOB).is_err());

        let reopened = CngKey::open_in(provider, &name).unwrap();
        assert_eq!(reopened.public_key(), key.public_key());
        drop((key, reopened));
        CngKey::delete_in(provider, &name).unwrap();
        assert!(CngKey::open_in(provider, &name).is_err());
    }

    #[test]
    fn a_software_ksp_key_signs_and_does_not_export() {
        sign_verify_and_refuse_export(Provider::Software);
    }

    /// Needs a TPM and is slow: `cargo test -p push-agent-key -- --ignored`.
    #[test]
    #[ignore]
    fn a_tpm_key_signs_and_does_not_export() {
        sign_verify_and_refuse_export(Provider::Platform);
    }

    #[test]
    fn a_missing_key_is_an_error_with_a_code() {
        let err = CngKey::open_in(Provider::Software, &test_key_name()).unwrap_err();
        assert_eq!(err.code, "NTE_BAD_KEYSET (0x80090016)");
        assert!(err.message.contains("NCryptOpenKey"));
    }

    #[test]
    fn open_classifies_a_missing_key() {
        for provider in Provider::ALL {
            for status in [NTE_BAD_KEYSET, NTE_NOT_FOUND, NTE_NO_KEY] {
                assert_eq!(
                    classify_open(provider, Step::Key, Some(status)),
                    OpenOutcome::Missing
                );
            }
        }
    }

    #[test]
    fn open_classifies_an_unavailable_provider() {
        for provider in Provider::ALL {
            assert_eq!(
                classify_open(provider, Step::OpenProvider, Some(NTE_PERM)),
                OpenOutcome::Unavailable
            );
        }
        for status in [
            NTE_DEVICE_NOT_FOUND,
            NTE_PROVIDER_DLL_FAIL,
            NTE_NOT_SUPPORTED,
        ] {
            assert_eq!(
                classify_open(Provider::Platform, Step::Key, Some(status)),
                OpenOutcome::Unavailable
            );
            assert_eq!(
                classify_open(Provider::Software, Step::Key, Some(status)),
                OpenOutcome::Failed
            );
        }
    }

    #[test]
    fn open_does_not_mistake_a_tpm_failure_for_a_missing_key() {
        for status in [
            NTE_DEVICE_NOT_READY,
            NTE_PERM,
            NTE_BAD_KEY_STATE,
            NTE_EXISTS,
        ] {
            assert_eq!(
                classify_open(Provider::Platform, Step::Key, Some(status)),
                OpenOutcome::Failed
            );
        }
        assert_eq!(
            classify_open(Provider::Platform, Step::Key, None),
            OpenOutcome::Failed
        );
    }

    #[test]
    fn create_reopens_a_key_someone_else_created() {
        for provider in Provider::ALL {
            assert_eq!(
                after_create_failure(provider, Step::Key, Some(NTE_EXISTS)),
                CreateNext::Reopen
            );
        }
    }

    #[test]
    fn create_falls_back_to_software_only_when_the_tpm_cannot_hold_the_key() {
        assert_eq!(
            after_create_failure(Provider::Platform, Step::OpenProvider, Some(NTE_PERM)),
            CreateNext::NextProvider
        );
        for status in [
            NTE_DEVICE_NOT_FOUND,
            NTE_PROVIDER_DLL_FAIL,
            NTE_NOT_SUPPORTED,
            NTE_BAD_ALGID,
        ] {
            assert_eq!(
                after_create_failure(Provider::Platform, Step::Key, Some(status)),
                CreateNext::NextProvider
            );
        }
        for status in [NTE_DEVICE_NOT_READY, NTE_PERM, NTE_BAD_KEYSET] {
            assert_eq!(
                after_create_failure(Provider::Platform, Step::Key, Some(status)),
                CreateNext::Stop
            );
        }
        assert_eq!(
            after_create_failure(Provider::Platform, Step::Key, None),
            CreateNext::Stop
        );
    }

    #[test]
    fn create_in_software_has_nowhere_else_to_go() {
        assert_eq!(
            after_create_failure(Provider::Software, Step::OpenProvider, Some(NTE_PERM)),
            CreateNext::Stop
        );
        assert_eq!(
            after_create_failure(Provider::Software, Step::Key, Some(NTE_NOT_SUPPORTED)),
            CreateNext::Stop
        );
    }

    #[test]
    fn a_name_with_nul_is_refused() {
        assert_eq!(
            CngKey::open_or_create("bad\0name").unwrap_err().code,
            "bad_key_name"
        );
        assert_eq!(CngKey::open("bad\0name").unwrap_err().code, "bad_key_name");
    }
}
