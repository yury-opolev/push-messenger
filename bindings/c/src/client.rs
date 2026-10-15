//! The client's calls (spec §8.2), one C function each: the account and the connection.

use std::ffi::c_char;
use std::time::Duration;

use push_sdk::DatabaseKey;
use push_sdk_ffi::json;

use crate::{PushClient, PushError, bytes, guard, handle, place, put_json, text, wait};

// ----- account -----

/// Signs up; auth-svc emails a verification link.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_up(
    client: *const PushClient,
    email: *const c_char,
    password: *const c_char,
    handle_name: *const c_char,
    display_name: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let (email, password) = unsafe { (text(email, "email")?, text(password, "password")?) };
        let (handle_name, display_name) = unsafe {
            (
                text(handle_name, "handle_name")?,
                text(display_name, "display_name")?,
            )
        };
        wait(
            this.client
                .sign_up(email, password, handle_name, display_name),
        )
    })
}

/// Sends the verification email again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_resend_verification(
    client: *const PushClient,
    email: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let email = unsafe { text(email, "email") }?;
        wait(this.client.resend_verification(email))
    })
}

/// Emails a password-reset link.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_forgot_password(
    client: *const PushClient,
    email: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let email = unsafe { text(email, "email") }?;
        wait(this.client.forgot_password(email))
    })
}

/// Signs in on this device (`platform`: `ios`, `android`, `web`, `desktop` or `other`);
/// `*account_json` gets `{"userId", "deviceId"}`. Then connect.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_in(
    client: *const PushClient,
    email: *const c_char,
    password: *const c_char,
    device_name: *const c_char,
    platform: *const c_char,
    account_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(account_json, "account_json")?;
        let this = unsafe { handle(client) }?;
        let (email, password) = unsafe { (text(email, "email")?, text(password, "password")?) };
        let (device_name, platform) = unsafe {
            (
                text(device_name, "device_name")?,
                text(platform, "platform")?,
            )
        };
        let platform = json::platform(platform)?;
        let account = wait(this.client.sign_in(email, password, device_name, platform))?;
        put_json(out, &json::account(&account));
        Ok(())
    })
}

/// The signed-in account as `{"userId", "deviceId"}`, or `null`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_account(
    client: *const PushClient,
    account_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(account_json, "account_json")?;
        let this = unsafe { handle(client) }?;
        let account = this.client.account()?;
        put_json(
            out,
            &account
                .as_ref()
                .map_or(serde_json::Value::Null, json::account),
        );
        Ok(())
    })
}

/// Signs this device out for good: forgets the credentials, erases the local data and asks
/// the server to revoke the device (spec §8.5).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_out(
    client: *const PushClient,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        wait(this.client.sign_out())
    })
}

// ----- connection -----

/// Connects, and keeps reconnecting until `push_client_disconnect`. Returns at once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_connect(
    client: *const PushClient,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || unsafe { handle(client) }?.client.connect())
}

/// Closes the connection and stops reconnecting. Returns at once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_disconnect(
    client: *const PushClient,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || unsafe { handle(client) }?.client.disconnect())
}

/// The connection's state as `{"state": …}` (`attempt` and `retryInMs` while
/// reconnecting).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_connection_state(
    client: *const PushClient,
    state_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(state_json, "state_json")?;
        let this = unsafe { handle(client) }?;
        put_json(out, &json::connection_state(this.client.connection_state()));
        Ok(())
    })
}

/// Waits until connected, up to `timeout_ms` (`offline` if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_wait_connected(
    client: *const PushClient,
    timeout_ms: u32,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let timeout = Duration::from_millis(u64::from(timeout_ms));
        wait(this.client.wait_connected(timeout))
    })
}

/// Encrypts the local database with a new key of `PUSH_DATABASE_KEY_LEN` bytes, or with
/// NULL decrypts it. The next open needs the new key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_change_database_key(
    client: *const PushClient,
    database_key: *const u8,
    database_key_len: usize,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let key = unsafe { bytes(database_key, database_key_len) }
            .map(DatabaseKey::from_bytes)
            .transpose()?;
        wait(this.client.change_database_key(key.as_ref()))
    })
}
