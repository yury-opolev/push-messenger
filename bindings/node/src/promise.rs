//! Promises settled from SDK threads.

use napi::bindgen_prelude::{Object, ToNapiValue};
use napi::{Env, JsDeferred, Status};
use push_sdk::SdkError;
use push_sdk_ffi::json;

type Resolver<T> = Box<dyn FnOnce(Env) -> napi::Result<T> + Send>;

/// A promise to settle from any thread. If it is dropped unsettled — the client closed
/// while the call ran, so its task was dropped — it rejects with `closed`: a promise never
/// stays pending, which would also keep Node's event loop alive.
pub struct Settle<T: ToNapiValue + Send + 'static> {
    deferred: Option<JsDeferred<T, Resolver<T>>>,
}

/// The rejection for `error`: an `Error` whose message is the error's JSON.
pub fn failure(error: &SdkError) -> napi::Error {
    napi::Error::new(Status::GenericFailure, json::error(error).to_string())
}

impl<T: ToNapiValue + Send + 'static> Settle<T> {
    pub fn new(env: &Env) -> napi::Result<(Self, Object<'_>)> {
        let (deferred, promise) = env.create_deferred()?;
        Ok((
            Self {
                deferred: Some(deferred),
            },
            promise,
        ))
    }

    pub fn finish(mut self, result: Result<T, SdkError>) {
        if let Some(deferred) = self.deferred.take() {
            match result {
                Ok(value) => deferred.resolve(Box::new(move |_| Ok(value))),
                Err(error) => deferred.reject(failure(&error)),
            }
        }
    }
}

impl<T: ToNapiValue + Send + 'static> Drop for Settle<T> {
    fn drop(&mut self) {
        if let Some(deferred) = self.deferred.take() {
            deferred.reject(failure(&SdkError::Closed));
        }
    }
}
