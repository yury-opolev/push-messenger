//! push-sdk's Node.js binding (spec §8.5), wrapped by `index.js`, which apps import.
//!
//! - **Promises.** Every call that talks to a server or touches the database returns a
//!   promise settled from the client's own runtime ([`push_sdk::Client::spawn`]): the
//!   JavaScript thread never waits for the SDK, and the binding has no runtime of its own.
//! - **Errors** reject (or throw) with an `Error` whose message is the JSON of
//!   [`push_sdk_ffi::json::error`]; `index.js` turns it into a `PushError` with the SDK's
//!   stable `code`.
//! - **Events** go to one listener through a threadsafe function ([`client`]).
//! - **The secret store** is the app's JavaScript object, bridged by [`secrets`].
//! - **Opening** runs on a thread of its own: it may stretch a database key and migrate the
//!   schema.

mod client;
mod ledger;
mod pending;
mod promise;
mod secrets;

use std::sync::Arc;

use napi::Env;
use napi::bindgen_prelude::{Buffer, Function, Object};
use napi_derive::napi;
use push_sdk::{Client, DatabaseKey, SECRET_KEYS, SdkError};
use push_sdk_ffi::json;
use serde_json::Value;

use crate::client::NativeClient;
use crate::promise::{Settle, failure};
use crate::secrets::{SecretArgs, SecretBridge};

/// The keys the SDK keeps in the app's secret store.
#[napi]
pub fn secret_keys() -> Vec<String> {
    SECRET_KEYS.iter().map(|key| (*key).to_owned()).collect()
}

/// Runs `work` on a thread of its own, settling the promise with its result.
fn on_thread<'env, T: napi::bindgen_prelude::ToNapiValue + Send + 'static>(
    env: &'env Env,
    name: &str,
    work: impl FnOnce() -> Result<T, SdkError> + Send + 'static,
) -> napi::Result<Object<'env>> {
    let (settle, promise) = Settle::new(env)?;
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || settle.finish(work()))
        .map_err(|_| failure(&SdkError::Invalid("cannot start a thread".to_owned())))?;
    Ok(promise)
}

/// Opens a client; resolves to a `NativeClient`. `config` is the JSON of
/// [`push_sdk_ffi::json::config`]; the database key comes apart, as bytes. `secret_call`
/// is the app's store, behind `index.js`'s bridge. The secrets are loaded and the client
/// opened on a thread of its own, never on the JavaScript thread.
#[napi(catch_unwind)]
pub fn open<'env>(
    env: &'env Env,
    config: Value,
    database_key: Option<Buffer>,
    secret_call: Function<'env, SecretArgs, ()>,
) -> napi::Result<Object<'env>> {
    let config = json::config(config, database_key.as_deref());
    let bridge = Arc::new(SecretBridge::new(&secret_call)?);
    on_thread(env, "push-sdk-open", move || {
        let config = config?;
        bridge.preload()?;
        Client::open(config, bridge).map(NativeClient::new)
    })
}

/// A database key derived from what the user knows and a salt the app keeps (Argon2id,
/// 19 MiB, 2 passes; tens of milliseconds, on a thread of its own); resolves to 32 bytes.
#[napi(catch_unwind)]
pub fn derive_database_key(env: &Env, secret: Buffer, salt: Buffer) -> napi::Result<Object<'_>> {
    let (secret, salt) = (secret.to_vec(), salt.to_vec());
    on_thread(env, "push-sdk-key", move || {
        DatabaseKey::derive(&secret, &salt).map(|key| Buffer::from(key.as_bytes().to_vec()))
    })
}

/// A new random salt for `deriveDatabaseKey`.
#[napi]
pub fn new_salt() -> Buffer {
    Buffer::from(DatabaseKey::new_salt().to_vec())
}

/// Hands the app's answer to a secret store call back to the SDK thread waiting for it.
#[napi(catch_unwind)]
pub fn complete_secret_call(id: u32, error: Option<String>, value: Option<Buffer>) {
    secrets::complete(id, error, value.map(|value| value.to_vec()));
}
