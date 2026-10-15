//! push-sdk's C binding (spec §8.5): a stable C ABI over [`push_sdk::Client`].
//!
//! - **Handles.** A `PushClient` is opaque: `push_client_open` creates it and
//!   `push_client_destroy` ends it. A `PushError` is opaque too, read with its accessors and
//!   freed with `push_error_free`.
//! - **Errors.** Every function that can fail returns [`PUSH_OK`] or [`PUSH_ERROR`] and, if
//!   the caller passes a place for it, an error whose code is the SDK's stable code
//!   ([`push_sdk::SdkError::code`]), the same in Rust, Node and C. A panic is caught at the
//!   boundary and reported as `panic`.
//! - **Strings.** In: UTF-8, NUL-terminated, borrowed for the call. Out: UTF-8 (JSON for
//!   anything with fields, shaped by [`push_sdk_ffi::json`]) that the caller owns and frees
//!   with `push_string_free`. JSON keeps the ABI stable as the SDK grows: a new field is
//!   not a new struct layout.
//! - **Blocking.** A call that talks to the servers blocks the calling thread until it ends;
//!   the SDK's own time limits bound every one (requests: 15 s for the connection and 15 s
//!   for the answer). Callbacks and completions would hand the app's context pointer to
//!   SDK threads for every call; blocking keeps ownership on the caller's stack, and a C
//!   app already has its own threads. Blocking calls are refused on the SDK's threads.
//! - **Events** come to one callback with a context pointer on an SDK thread
//!   (`push_client_set_event_callback`), or are polled (`push_client_next_event`).
//! - **The secret store** is the app's, as C function pointers ([`PushSecretStore`]).

use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use push_sdk::{Client, DatabaseKey, Events, SdkError, SecretError, SecretStore};
use push_sdk_ffi::{block_on, json, on_sdk_thread};
use serde_json::Value;

mod client;

pub use client::*;

/// The call succeeded.
pub const PUSH_OK: i32 = 0;
/// The call failed; the error says why.
pub const PUSH_ERROR: i32 = -1;
/// A secret store's `load`: there is no value under the key.
pub const PUSH_NOT_FOUND: i32 = 1;
/// The length of a database key, in bytes.
pub const PUSH_DATABASE_KEY_LEN: usize = 32;
/// The length of a salt `push_new_salt` makes, in bytes.
pub const PUSH_SALT_LEN: usize = 16;

/// Why a call failed. Read it with `push_error_code`, `push_error_message`,
/// `push_error_retryable` and `push_error_retry_after_s`; free it with `push_error_free`.
pub struct PushError {
    code: CString,
    message: CString,
    retryable: bool,
    retry_after_s: i32,
}

impl PushError {
    fn new(error: &SdkError) -> Self {
        let shown = json::error(error);
        let text = |name: &str| {
            let value = shown[name].as_str().unwrap_or_default();
            CString::new(value.replace('\0', " ")).unwrap_or_default()
        };
        let retry_after_s = shown["retryAfterS"]
            .as_i64()
            .and_then(|seconds| i32::try_from(seconds).ok())
            .unwrap_or(-1);
        Self {
            code: text("code"),
            message: text("message"),
            retryable: shown["retryable"].as_bool().unwrap_or(false),
            retry_after_s,
        }
    }

    fn panic() -> Self {
        Self {
            code: c"panic".to_owned(),
            message: c"the SDK failed unexpectedly (a bug); destroy the client".to_owned(),
            retryable: false,
            retry_after_s: -1,
        }
    }
}

/// The app's secret store (spec §8.2), as C function pointers. The SDK calls them on its
/// own threads and on the threads that call it, so they must be thread-safe, and they must
/// not call the SDK (those calls fail). Every function gets `context` first.
///
/// - `load` stores a value the app allocated in `*value` and `*value_len` and returns
///   `PUSH_OK`, or returns `PUSH_NOT_FOUND`, or `PUSH_ERROR`. The SDK copies the value and
///   hands it back to `free_value`.
/// - `save` replaces the value under `key` (`value_len` bytes, borrowed for the call).
/// - `remove` deletes it; removing a missing key succeeds.
/// - `release`, if set, is called once when the SDK no longer needs the store: after
///   `push_client_destroy`, or when `push_client_open` fails.
///
/// The keys are those of `SECRET_KEYS` in push-sdk: `push.session`, `push.device` and
/// `push.registration`. Values are opaque bytes. `docs/sdk-secure-storage.md` shows how to
/// keep them in the platform's keychain.
#[repr(C)]
pub struct PushSecretStore {
    pub context: *mut c_void,
    pub load: Option<
        unsafe extern "C" fn(
            context: *mut c_void,
            key: *const c_char,
            value: *mut *mut u8,
            value_len: *mut usize,
        ) -> i32,
    >,
    pub save: Option<
        unsafe extern "C" fn(
            context: *mut c_void,
            key: *const c_char,
            value: *const u8,
            value_len: usize,
        ) -> i32,
    >,
    pub remove: Option<unsafe extern "C" fn(context: *mut c_void, key: *const c_char) -> i32>,
    pub free_value:
        Option<unsafe extern "C" fn(context: *mut c_void, value: *mut u8, value_len: usize)>,
    pub release: Option<unsafe extern "C" fn(context: *mut c_void)>,
}

thread_local! {
    /// Set while this thread runs one of the app's secret store functions.
    static IN_STORE: Cell<bool> = const { Cell::new(false) };
}

/// The app's store, as the SDK sees it.
struct CStore(PushSecretStore);

// The app promises thread-safe functions and context (documented on PushSecretStore).
unsafe impl Send for CStore {}
unsafe impl Sync for CStore {}

impl CStore {
    /// Runs one of the app's functions, marked so that a call back into the SDK from it is
    /// refused.
    fn call<T>(&self, call: impl FnOnce() -> T) -> T {
        IN_STORE.with(|flag| flag.set(true));
        let result = call();
        IN_STORE.with(|flag| flag.set(false));
        result
    }

    fn key(key: &str) -> Result<CString, SecretError> {
        CString::new(key).map_err(|_| SecretError("a key holds a NUL byte".to_owned()))
    }
}

impl SecretStore for CStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        let load = self
            .0
            .load
            .ok_or_else(|| SecretError("no load function".to_owned()))?;
        let key = Self::key(key)?;
        let (mut value, mut value_len) = (ptr::null_mut::<u8>(), 0usize);
        let status =
            self.call(|| unsafe { load(self.0.context, key.as_ptr(), &mut value, &mut value_len) });
        match status {
            PUSH_NOT_FOUND => Ok(None),
            PUSH_OK => {
                let copied = if value.is_null() {
                    Vec::new()
                } else {
                    // The app gave `value_len` bytes at `value`, valid until free_value.
                    unsafe { std::slice::from_raw_parts(value, value_len) }.to_vec()
                };
                if let (Some(free_value), false) = (self.0.free_value, value.is_null()) {
                    self.call(|| unsafe {
                        free_value(self.0.context, value, value_len);
                    });
                }
                Ok(Some(copied))
            }
            _ => Err(SecretError(
                "the app's secret store could not load".to_owned(),
            )),
        }
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        let save = self
            .0
            .save
            .ok_or_else(|| SecretError("no save function".to_owned()))?;
        let key = Self::key(key)?;
        let status = self
            .call(|| unsafe { save(self.0.context, key.as_ptr(), value.as_ptr(), value.len()) });
        match status {
            PUSH_OK => Ok(()),
            _ => Err(SecretError(
                "the app's secret store could not save".to_owned(),
            )),
        }
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let remove = self
            .0
            .remove
            .ok_or_else(|| SecretError("no remove function".to_owned()))?;
        let key = Self::key(key)?;
        let status = self.call(|| unsafe { remove(self.0.context, key.as_ptr()) });
        match status {
            PUSH_OK => Ok(()),
            _ => Err(SecretError(
                "the app's secret store could not remove".to_owned(),
            )),
        }
    }
}

impl Drop for CStore {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            unsafe { release(self.0.context) };
        }
    }
}

/// Called with each event as UTF-8 JSON (`{"type": …}`), borrowed for the call, on an SDK
/// thread of its own, one event at a time and in order. It may call the SDK, including
/// blocking calls and `push_client_destroy`.
pub type PushEventCallback =
    Option<unsafe extern "C" fn(context: *mut c_void, event_json: *const c_char)>;

/// Called once, on the events thread, after the last event: only now may the context be
/// freed. `push_client_destroy` does not wait for it: events already queued can still reach
/// the event callback after `push_client_destroy` returns, so `context` must stay valid
/// until this is called.
pub type PushReleaseCallback = Option<unsafe extern "C" fn(context: *mut c_void)>;

/// How the app takes the events: not yet, by polling, or through a callback.
enum Delivery {
    Waiting(Events),
    Polled(Arc<tokio::sync::Mutex<Events>>),
    Called,
}

/// A client: opaque to C.
pub struct PushClient {
    client: Client,
    events: Mutex<Delivery>,
}

/// A pointer an SDK thread may carry: the app's context, which it promised is thread-safe.
struct Context(*mut c_void);

unsafe impl Send for Context {}

/// Runs `body` at the boundary: no panic crosses it, a failure becomes a `PushError` in
/// `*error` (when the app gave a place for it). Calls from inside a secret store function
/// are refused by `place`, `buffer` and `handle`, which every call starts with.
fn guard(error: *mut *mut PushError, body: impl FnOnce() -> Result<(), SdkError>) -> i32 {
    if !error.is_null() {
        // The app gave a place for an error: start it empty.
        unsafe { *error = ptr::null_mut() };
    }
    let outcome = catch_unwind(AssertUnwindSafe(body));
    let failure = match outcome {
        Ok(Ok(())) => return PUSH_OK,
        Ok(Err(sdk_error)) => PushError::new(&sdk_error),
        Err(_) => PushError::panic(),
    };
    if !error.is_null() {
        unsafe { *error = Box::into_raw(Box::new(failure)) };
    }
    PUSH_ERROR
}

/// Waits for `future` on the calling thread; refused on the SDK's own threads, where the
/// wait could hold up the answer.
fn wait<T>(future: impl Future<Output = Result<T, SdkError>>) -> Result<T, SdkError> {
    if on_sdk_thread() {
        return Err(SdkError::Invalid(
            "a blocking call on an SDK thread: call it from a thread of the app".to_owned(),
        ));
    }
    block_on(future)
}

/// A string argument, borrowed for the call.
unsafe fn text<'a>(value: *const c_char, what: &str) -> Result<&'a str, SdkError> {
    if value.is_null() {
        return Err(SdkError::Invalid(format!("{what} is missing")));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| SdkError::Invalid(format!("{what} is not UTF-8")))
}

/// Bytes an argument points to; none for NULL.
unsafe fn bytes<'a>(value: *const u8, len: usize) -> Option<&'a [u8]> {
    if value.is_null() {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(value, len) })
}

/// Refuses a call made from inside one of the app's secret store functions: it could wait
/// for the store's own caller.
fn refuse_in_store() -> Result<(), SdkError> {
    if IN_STORE.with(Cell::get) {
        return Err(SdkError::Invalid(
            "the SDK was called from a secret store function".to_owned(),
        ));
    }
    Ok(())
}

/// Checks a place for an owned result (a handle or a string) before any work is done, so a
/// call never acts and then fails to report, and sets it to NULL at once: whatever the
/// call's outcome, a failed call leaves NULL there and the app never frees garbage. Every
/// call with such a place starts with this, before it checks anything else.
fn place<T>(out: *mut *mut T, what: &str) -> Result<*mut *mut T, SdkError> {
    if out.is_null() {
        return Err(SdkError::Invalid(format!("{what} is NULL")));
    }
    unsafe { *out = ptr::null_mut() };
    refuse_in_store()?;
    Ok(out)
}

/// Checks a place for plain data (key or salt bytes) before any work is done.
fn buffer<T>(out: *mut T, what: &str) -> Result<*mut T, SdkError> {
    if out.is_null() {
        return Err(SdkError::Invalid(format!("{what} is NULL")));
    }
    refuse_in_store()?;
    Ok(out)
}
fn owned(value: String) -> *mut c_char {
    CString::new(value.replace('\0', " "))
        .unwrap_or_default()
        .into_raw()
}

/// Stores `value` as JSON in `*out`, for the caller to free with `push_string_free`.
fn put_json(out: *mut *mut c_char, value: &Value) {
    unsafe { *out = owned(value.to_string()) };
}

unsafe fn handle<'a>(client: *const PushClient) -> Result<&'a PushClient, SdkError> {
    refuse_in_store()?;
    if client.is_null() {
        return Err(SdkError::Invalid("the client is NULL".to_owned()));
    }
    Ok(unsafe { &*client })
}

/// The version of the SDK, as a static string (never freed).
#[unsafe(no_mangle)]
pub extern "C" fn push_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The error's stable code (`timeout`, `wrong_key`, the server's `not_contact`, …),
/// valid until `push_error_free`. NULL for a NULL error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_code(error: *const PushError) -> *const c_char {
    match unsafe { error.as_ref() } {
        Some(error) => error.code.as_ptr(),
        None => ptr::null(),
    }
}

/// A sentence for logs, never containing a token, a key or a message's text; valid until
/// `push_error_free`. NULL for a NULL error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_message(error: *const PushError) -> *const c_char {
    match unsafe { error.as_ref() } {
        Some(error) => error.message.as_ptr(),
        None => ptr::null(),
    }
}

/// Whether the same call may succeed later, unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_retryable(error: *const PushError) -> bool {
    unsafe { error.as_ref() }.is_some_and(|error| error.retryable)
}

/// Rate limited: after how many seconds the same call may succeed; -1 if not said.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_retry_after_s(error: *const PushError) -> i32 {
    unsafe { error.as_ref() }.map_or(-1, |error| error.retry_after_s)
}

/// Frees an error; NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_free(error: *mut PushError) {
    if !error.is_null() {
        drop(unsafe { Box::from_raw(error) });
    }
}

/// Frees a string the SDK returned; NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}

/// Opens a client: the local database (SQLite at `databasePath`) and the SDK's runtime.
/// It does not connect until `push_client_connect`. May take a second (a database key is
/// checked, the schema migrated): call it off the UI thread.
///
/// `config_json`: `{"authUrl", "socketUrl", "databasePath", "host"?, "requestTimeoutMs"?}`.
/// `database_key`: NULL for a plain database, or `PUSH_DATABASE_KEY_LEN` bytes that
/// encrypt the whole file (`wrong_key` if they do not open it); copied, never stored or
/// logged. `store`: the app's secret store, owned by the client from now on (released
/// even when this fails). `*client` gets the client.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_open(
    config_json: *const c_char,
    database_key: *const u8,
    database_key_len: usize,
    store: PushSecretStore,
    client: *mut *mut PushClient,
    error: *mut *mut PushError,
) -> i32 {
    // Owned first, so the store is released on every path.
    let store = CStore(store);
    guard(error, move || {
        let out = place(client, "client")?;
        let config = unsafe { text(config_json, "config_json") }?;
        let config: Value = serde_json::from_str(config)
            .map_err(|_| SdkError::Invalid("config_json is not JSON".to_owned()))?;
        let key = unsafe { bytes(database_key, database_key_len) };
        let config = json::config(config, key)?;
        let (opened, events) = Client::open(config, Arc::new(store))?;
        let handle = PushClient {
            client: opened,
            events: Mutex::new(Delivery::Waiting(events)),
        };
        unsafe { *out = Box::into_raw(Box::new(handle)) };
        Ok(())
    })
}

/// Closes the client and frees it: the connection stops, and the database is closed and
/// the secret store released before this returns (it waits up to a few seconds), so the
/// file can be opened again at once (also from the event callback). Called on one of the
/// SDK's runtime threads (from a secret store function) it does not wait. NULL is ignored.
/// No other call may use the client during or after this. It does not wait for the event
/// callback: events already queued can still reach it after this returns, until its
/// `release` is called, so the callback's context must outlive this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_destroy(client: *mut PushClient) {
    if client.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let handle = unsafe { Box::from_raw(client) };
        let PushClient { client, events } = *handle;
        drop(events);
        if on_sdk_thread() {
            drop(client);
        } else {
            let _ = block_on(client.close());
        }
    }));
}

/// Delivers every event to `callback` with `context`, on an SDK thread of its own, one at a
/// time and in order, until the client is destroyed and the events queued by then are
/// delivered; then `release` (if set) is called once with `context`. The callback can
/// still run after `push_client_destroy` returns, up to the call of `release`: keep
/// `context` valid until `release` runs (without a `release`, for as long as the process
/// lives). Events wait in the SDK (at most 10,000; then one `eventsLost`)
/// until this is called. Only once per client, and not after `push_client_next_event`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_set_event_callback(
    client: *const PushClient,
    callback: PushEventCallback,
    context: *mut c_void,
    release: PushReleaseCallback,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let handle = unsafe { handle(client) }?;
        let callback =
            callback.ok_or_else(|| SdkError::Invalid("the callback is NULL".to_owned()))?;
        let mut delivery = handle.events.lock().map_err(|_| SdkError::Closed)?;
        let Delivery::Waiting(_) = &*delivery else {
            return Err(SdkError::Invalid("the events are taken already".to_owned()));
        };
        let Delivery::Waiting(mut events) = std::mem::replace(&mut *delivery, Delivery::Called)
        else {
            unreachable!("checked above");
        };
        let context = Context(context);
        std::thread::Builder::new()
            .name("push-sdk-events".to_owned())
            .spawn(move || {
                let context = context;
                while let Some(event) = block_on(events.next()) {
                    let shown = CString::new(json::event(&event).to_string()).unwrap_or_default();
                    unsafe { callback(context.0, shown.as_ptr()) };
                }
                if let Some(release) = release {
                    unsafe { release(context.0) };
                }
            })
            .map_err(|_| SdkError::Invalid("cannot start the events thread".to_owned()))?;
        Ok(())
    })
}

/// The next event as JSON in `*event_json` (the caller frees it), waiting up to
/// `timeout_ms`; NULL in `*event_json` if none came in time. Not after
/// `push_client_set_event_callback`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_next_event(
    client: *const PushClient,
    timeout_ms: u32,
    event_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(event_json, "event_json")?;
        let handle = unsafe { handle(client) }?;
        if on_sdk_thread() {
            return Err(SdkError::Invalid(
                "a blocking call on an SDK thread: call it from a thread of the app".to_owned(),
            ));
        }
        let events = {
            let mut delivery = handle.events.lock().map_err(|_| SdkError::Closed)?;
            if let Delivery::Waiting(_) = &*delivery {
                let Delivery::Waiting(events) = std::mem::replace(&mut *delivery, Delivery::Called)
                else {
                    unreachable!("checked above");
                };
                *delivery = Delivery::Polled(Arc::new(tokio::sync::Mutex::new(events)));
            }
            match &*delivery {
                Delivery::Polled(events) => events.clone(),
                _ => {
                    return Err(SdkError::Invalid("the events go to a callback".to_owned()));
                }
            }
        };
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        handle.client.spawn(async move {
            let mut events = events.lock().await;
            let next =
                tokio::time::timeout(Duration::from_millis(u64::from(timeout_ms)), events.next())
                    .await;
            let _ = sender.send(next);
        })?;
        match receiver.recv() {
            Ok(Ok(Some(event))) => put_json(out, &json::event(&event)),
            Ok(Err(_)) => unsafe { *out = ptr::null_mut() },
            Ok(Ok(None)) | Err(_) => return Err(SdkError::Closed),
        }
        Ok(())
    })
}

/// Derives a database key from what the user knows (a PIN, a passphrase) and a salt of at
/// least `PUSH_SALT_LEN` bytes the app keeps next to the database (Argon2id, 19 MiB, 2
/// passes): the same input always gives the same key. Writes `PUSH_DATABASE_KEY_LEN`
/// bytes to `key`. Takes tens of milliseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_derive_database_key(
    secret: *const u8,
    secret_len: usize,
    salt: *const u8,
    salt_len: usize,
    key: *mut u8,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = buffer(key, "key")?;
        let secret = unsafe { bytes(secret, secret_len) }
            .ok_or_else(|| SdkError::Invalid("the secret is NULL".to_owned()))?;
        let salt = unsafe { bytes(salt, salt_len) }
            .ok_or_else(|| SdkError::Invalid("the salt is NULL".to_owned()))?;
        let derived = DatabaseKey::derive(secret, salt)?;
        let derived = derived.as_bytes();
        unsafe { ptr::copy_nonoverlapping(derived.as_ptr(), out, PUSH_DATABASE_KEY_LEN) };
        Ok(())
    })
}

/// Writes a new random salt of `PUSH_SALT_LEN` bytes to `salt`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_new_salt(salt: *mut u8) -> i32 {
    guard(ptr::null_mut(), || {
        let out = buffer(salt, "salt")?;
        let made = DatabaseKey::new_salt();
        unsafe { ptr::copy_nonoverlapping(made.as_ptr(), out, PUSH_SALT_LEN) };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    use push_sdk::testing::{AutoGateway, FakeAuth};

    use super::*;

    fn code(error: *mut PushError) -> String {
        let code = unsafe { CStr::from_ptr(push_error_code(error)) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { push_error_free(error) };
        code
    }

    #[test]
    fn a_panic_stops_at_the_boundary() {
        let mut error = ptr::null_mut();
        let status = guard(&mut error, || panic!("a bug"));
        assert_eq!(status, PUSH_ERROR);
        assert_eq!(code(error), "panic");
    }

    /// A C app frees what a call hands back, so a failed call must leave NULL, not what the
    /// place held before.
    #[test]
    fn a_failed_call_leaves_null_in_every_place_for_a_result() {
        let mut error = ptr::null_mut();
        let mut account = ptr::without_provenance_mut::<c_char>(8);
        let status = unsafe { push_client_account(ptr::null(), &mut account, &mut error) };
        assert_eq!((status, code(error)), (PUSH_ERROR, "invalid".to_owned()));
        assert!(account.is_null());

        // A failure after the place was checked: a config that is not JSON.
        let mut client = ptr::without_provenance_mut::<PushClient>(8);
        let store = PushSecretStore {
            context: ptr::null_mut(),
            load: None,
            save: None,
            remove: None,
            free_value: None,
            release: None,
        };
        let status = unsafe {
            push_client_open(
                c"{".as_ptr(),
                ptr::null(),
                0,
                store,
                &mut client,
                &mut error,
            )
        };
        assert_eq!((status, code(error)), (PUSH_ERROR, "invalid".to_owned()));
        assert!(client.is_null());
    }

    #[test]
    fn nulls_are_refused_before_anything_happens() {
        let mut error = ptr::null_mut();
        let status = unsafe { push_client_connect(ptr::null(), &mut error) };
        assert_eq!(status, PUSH_ERROR);
        assert_eq!(code(error), "invalid");
        // No place for the error: the status alone tells.
        assert_eq!(
            unsafe { push_client_connect(ptr::null(), ptr::null_mut()) },
            PUSH_ERROR
        );
        unsafe {
            push_string_free(ptr::null_mut());
            push_error_free(ptr::null_mut());
            push_client_destroy(ptr::null_mut());
        }
        assert!(unsafe { push_error_code(ptr::null()) }.is_null());
    }

    /// The client of the store's tests: set by the test before the store is called.
    static CLIENT: AtomicPtr<PushClient> = AtomicPtr::new(ptr::null_mut());
    static REFUSED: AtomicUsize = AtomicUsize::new(0);

    /// A store that calls back into the SDK from `save`, which the SDK must refuse rather
    /// than wait for itself.
    unsafe extern "C" fn reentrant_save(
        _context: *mut c_void,
        _key: *const c_char,
        _value: *const u8,
        _len: usize,
    ) -> i32 {
        let client = CLIENT.load(Ordering::SeqCst);
        let mut account = ptr::without_provenance_mut::<c_char>(8);
        let mut error = ptr::null_mut();
        if unsafe { push_client_account(client, &mut account, &mut error) } == PUSH_ERROR
            && code(error) == "invalid"
            && account.is_null()
        {
            REFUSED.fetch_add(1, Ordering::SeqCst);
        }
        PUSH_OK
    }

    unsafe extern "C" fn empty_load(
        _context: *mut c_void,
        _key: *const c_char,
        _value: *mut *mut u8,
        _len: *mut usize,
    ) -> i32 {
        PUSH_NOT_FOUND
    }

    unsafe extern "C" fn removed(_context: *mut c_void, _key: *const c_char) -> i32 {
        PUSH_OK
    }

    #[test]
    fn the_sdk_refuses_calls_from_its_own_threads_and_from_the_store() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (auth, gateway) = runtime.block_on(async {
            let auth = FakeAuth::start().await;
            let gateway = AutoGateway::start(auth.identity.clone()).await;
            (auth, gateway)
        });
        let config = CString::new(format!(
            r#"{{"authUrl":"{}","socketUrl":"{}","databasePath":":memory:"}}"#,
            auth.url, gateway.url
        ))
        .unwrap();
        let store = PushSecretStore {
            context: ptr::null_mut(),
            load: Some(empty_load),
            save: Some(reentrant_save),
            remove: Some(removed),
            free_value: None,
            release: None,
        };
        let (mut client, mut error) = (ptr::null_mut(), ptr::null_mut());
        let opened = unsafe {
            push_client_open(
                config.as_ptr(),
                ptr::null(),
                0,
                store,
                &mut client,
                &mut error,
            )
        };
        assert_eq!(opened, PUSH_OK);
        CLIENT.store(client, Ordering::SeqCst);
        let mut account = ptr::null_mut();
        let signed_in = unsafe {
            push_client_sign_in(
                client,
                c"a@example.com".as_ptr(),
                c"pw".as_ptr(),
                c"phone".as_ptr(),
                c"web".as_ptr(),
                &mut account,
                &mut error,
            )
        };
        assert_eq!(signed_in, PUSH_OK);
        unsafe { push_string_free(account) };
        assert!(
            REFUSED.load(Ordering::SeqCst) >= 1,
            "the store's call came back refused"
        );

        // A blocking call on one of the SDK's threads is refused instead of waiting there.
        let (sender, receiver) = std::sync::mpsc::channel();
        let shared = CLIENT.load(Ordering::SeqCst) as usize;
        unsafe { &*client }
            .client
            .spawn(async move {
                let mut error = ptr::null_mut();
                let client = shared as *const PushClient;
                let status = unsafe {
                    push_client_forgot_password(client, c"a@example.com".as_ptr(), &mut error)
                };
                let _ = sender.send((status, code(error)));
            })
            .unwrap();
        assert_eq!(receiver.recv().unwrap(), (PUSH_ERROR, "invalid".to_owned()));
        unsafe { push_client_destroy(client) };
        drop(runtime);
    }
}
