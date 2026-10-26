//! The client's calls (spec §8.2), one C function each.

use std::ffi::c_char;
use std::time::Duration;

use push_sdk::{ClientMsgId, ConversationId, DatabaseKey, DeviceId, SdkError, UserId};
use push_sdk_ffi::json;

use crate::{
    PushClient, PushError, buffer, bytes, guard, handle, owned, place, put_json, text, wait,
};

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

/// Signs in on this device with a Microsoft Entra ID access token the app obtained (MSAL);
/// `handle_name` (may be NULL) names the account on its first sign-in only. `*account_json`
/// gets `{"userId", "deviceId"}`. Then connect. The token is never stored or logged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_in_with_entra(
    client: *const PushClient,
    access_token: *const c_char,
    handle_name: *const c_char,
    device_name: *const c_char,
    platform: *const c_char,
    account_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(account_json, "account_json")?;
        let this = unsafe { handle(client) }?;
        let access_token = unsafe { text(access_token, "access_token") }?;
        let handle_name = if handle_name.is_null() {
            None
        } else {
            Some(unsafe { text(handle_name, "handle_name") }?)
        };
        let (device_name, platform) = unsafe {
            (
                text(device_name, "device_name")?,
                text(platform, "platform")?,
            )
        };
        let platform = json::platform(platform)?;
        let account =
            wait(
                this.client
                    .sign_in_with_entra(access_token, handle_name, device_name, platform),
            )?;
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
/// NULL decrypts it (a non-zero length with a NULL key is `invalid`). The next open needs the new key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_change_database_key(
    client: *const PushClient,
    database_key: *const u8,
    database_key_len: usize,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let key = unsafe { bytes(database_key, database_key_len) }?
            .map(DatabaseKey::from_bytes)
            .transpose()?;
        wait(this.client.change_database_key(key.as_ref()))
    })
}

// ----- local data -----

unsafe fn conversation_id(value: *const c_char) -> Result<ConversationId, SdkError> {
    json::id(
        unsafe { text(value, "conversation_id") }?,
        "conversation_id",
    )
}

unsafe fn user_id(value: *const c_char) -> Result<UserId, SdkError> {
    json::id(unsafe { text(value, "user_id") }?, "user_id")
}

unsafe fn client_msg_id(value: *const c_char, what: &str) -> Result<ClientMsgId, SdkError> {
    json::id(unsafe { text(value, what) }?, what)
}

/// Every conversation this device knows, as a JSON array, most recently changed first.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_conversations(
    client: *const PushClient,
    conversations_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(conversations_json, "conversations_json")?;
        let this = unsafe { handle(client) }?;
        let conversations = this.client.conversations()?;
        put_json(out, &json::list(&conversations, json::conversation));
        Ok(())
    })
}

/// One conversation as JSON, or `null` if this device does not know it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_conversation(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    conversation_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(conversation_json, "conversation_json")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let found = this.client.conversation(id)?;
        put_json(
            out,
            &found
                .as_ref()
                .map_or(serde_json::Value::Null, json::conversation),
        );
        Ok(())
    })
}

/// Up to `limit` stored messages before `*before_seq` (NULL: the newest, then those still
/// in the outbox), oldest first, as a JSON array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_messages(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    before_seq: *const i64,
    limit: u32,
    messages_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(messages_json, "messages_json")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let before = unsafe { before_seq.as_ref() }.copied();
        let messages = this.client.messages(id, before, limit)?;
        put_json(out, &json::list(&messages, json::message));
        Ok(())
    })
}

/// The user's relationships as this device knows them, as a JSON array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_contacts(
    client: *const PushClient,
    contacts_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(contacts_json, "contacts_json")?;
        let this = unsafe { handle(client) }?;
        let contacts = this.client.contacts()?;
        put_json(out, &json::list(&contacts, json::contact));
        Ok(())
    })
}

// ----- sending -----

/// Queues a text message; `*message_json` gets the local echo (its `clientMsgId` is the
/// handle for edit, delete and cancel). Sent once connected, in order.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_send(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    message_text: *const c_char,
    message_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(message_json, "message_json")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let body = unsafe { text(message_text, "text") }?;
        let message = this.client.send(id, body)?;
        put_json(out, &json::message(&message));
        Ok(())
    })
}

/// Queues a card: `rich_json` is a RICH body `{"tpl", "params", "fallback"}`, checked as
/// an envelope (`invalid` otherwise). `*message_json` gets the local echo, as with
/// `push_client_send`; the server checks the template and may refuse it later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_send_rich(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    rich_json: *const c_char,
    message_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(message_json, "message_json")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let body = unsafe { text(rich_json, "rich_json") }?;
        let message = this.client.send_rich(id, body)?;
        put_json(out, &json::message(&message));
        Ok(())
    })
}

/// Queues an answer to a card: `answer_json` is an ANSWER body `{"card", "values",
/// "fallback"}` naming a card held in the conversation, from someone else and not
/// answered yet (`invalid` otherwise). `*message_json` gets the local echo.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_send_answer(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    answer_json: *const c_char,
    message_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(message_json, "message_json")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let body = unsafe { text(answer_json, "answer_json") }?;
        let message = this.client.send_answer(id, body)?;
        put_json(out, &json::message(&message));
        Ok(())
    })
}

/// Changes the text of one of the user's messages, or a card's RICH body (not an answer,
/// nor an answered card); `*change_id` gets the id of the change.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_edit(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    target_client_msg_id: *const c_char,
    message_text: *const c_char,
    change_id: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(change_id, "change_id")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let target = unsafe { client_msg_id(target_client_msg_id, "target_client_msg_id") }?;
        let body = unsafe { text(message_text, "text") }?;
        let change = this.client.edit(id, target, body)?;
        unsafe { *out = owned(change.to_string()) };
        Ok(())
    })
}

/// Deletes one of the user's messages; `*change_id` gets the id of the change.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_delete(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    target_client_msg_id: *const c_char,
    change_id: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(change_id, "change_id")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let target = unsafe { client_msg_id(target_client_msg_id, "target_client_msg_id") }?;
        let change = this.client.delete(id, target)?;
        unsafe { *out = owned(change.to_string()) };
        Ok(())
    })
}

/// Takes back a message never sent; `*cancelled` says whether it was.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_cancel(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    message_client_msg_id: *const c_char,
    cancelled: *mut bool,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = buffer(cancelled, "cancelled")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let message = unsafe { client_msg_id(message_client_msg_id, "client_msg_id") }?;
        let taken_back = this.client.cancel(id, message)?;
        unsafe { *out = taken_back };
        Ok(())
    })
}

/// Marks the conversation read up to `seq`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_mark_read(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    seq: i64,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        this.client.mark_read(id, seq)
    })
}

// ----- requests (blocking) -----

/// Lists the conversations from the server, stores what changed and returns all, as a
/// JSON array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_list_conversations(
    client: *const PushClient,
    conversations_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(conversations_json, "conversations_json")?;
        let this = unsafe { handle(client) }?;
        let conversations = wait(this.client.list_conversations())?;
        put_json(out, &json::list(&conversations, json::conversation));
        Ok(())
    })
}

/// Loads up to `limit` older messages before `*before_seq` (NULL: the newest) from the
/// server and stores them; oldest first, as a JSON array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_fetch_history(
    client: *const PushClient,
    conversation_id_text: *const c_char,
    before_seq: *const i64,
    limit: u32,
    messages_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(messages_json, "messages_json")?;
        let this = unsafe { handle(client) }?;
        let id = unsafe { conversation_id(conversation_id_text) }?;
        let before = unsafe { before_seq.as_ref() }.copied();
        let messages = wait(this.client.fetch_history(id, before, limit))?;
        put_json(out, &json::list(&messages, json::message));
        Ok(())
    })
}

/// The direct conversation with an accepted contact, created the first time, as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_open_direct(
    client: *const PushClient,
    peer_user_id: *const c_char,
    conversation_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(conversation_json, "conversation_json")?;
        let this = unsafe { handle(client) }?;
        let peer = unsafe { user_id(peer_user_id) }?;
        let conversation = wait(this.client.open_direct(peer))?;
        put_json(out, &json::conversation(&conversation));
        Ok(())
    })
}

/// Lists the user's relationships from the server, as a JSON array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_list_contacts(
    client: *const PushClient,
    contacts_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(contacts_json, "contacts_json")?;
        let this = unsafe { handle(client) }?;
        let contacts = wait(this.client.list_contacts())?;
        put_json(out, &json::list(&contacts, json::contact));
        Ok(())
    })
}

/// What a contact call does.
#[derive(Clone, Copy)]
enum ContactCall {
    Request,
    Accept,
    Decline,
    Block,
    Unblock,
}

unsafe fn contact_call(
    client: *const PushClient,
    call: ContactCall,
    user: *const c_char,
    contact_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(contact_json, "contact_json")?;
        let this = unsafe { handle(client) }?;
        let user = unsafe { user_id(user) }?;
        let client = &this.client;
        let contact = wait(async move {
            match call {
                ContactCall::Request => client.request_contact(user).await,
                ContactCall::Accept => client.accept_contact(user).await,
                ContactCall::Decline => client.decline_contact(user).await,
                ContactCall::Block => client.block(user).await,
                ContactCall::Unblock => client.unblock(user).await,
            }
        })?;
        put_json(out, &json::contact(&contact));
        Ok(())
    })
}

/// Asks another user to become a contact; `*contact_json` gets the relationship now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_request_contact(
    client: *const PushClient,
    user: *const c_char,
    contact_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    unsafe { contact_call(client, ContactCall::Request, user, contact_json, error) }
}

/// Accepts a contact request.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_accept_contact(
    client: *const PushClient,
    user: *const c_char,
    contact_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    unsafe { contact_call(client, ContactCall::Accept, user, contact_json, error) }
}

/// Declines a contact request.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_decline_contact(
    client: *const PushClient,
    user: *const c_char,
    contact_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    unsafe { contact_call(client, ContactCall::Decline, user, contact_json, error) }
}

/// Blocks a user.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_block(
    client: *const PushClient,
    user: *const c_char,
    contact_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    unsafe { contact_call(client, ContactCall::Block, user, contact_json, error) }
}

/// Unblocks a user.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_unblock(
    client: *const PushClient,
    user: *const c_char,
    contact_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    unsafe { contact_call(client, ContactCall::Unblock, user, contact_json, error) }
}

/// Creates an invite code valid for `expires_in_s` seconds and `max_uses` uses, as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_create_invite(
    client: *const PushClient,
    expires_in_s: u32,
    max_uses: u32,
    invite_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(invite_json, "invite_json")?;
        let this = unsafe { handle(client) }?;
        let invite = wait(this.client.create_invite(expires_in_s, max_uses))?;
        put_json(out, &json::invite(&invite));
        Ok(())
    })
}

/// Redeems an invite code; `*inviter_user_id` gets its creator, now a contact.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_redeem_invite(
    client: *const PushClient,
    code: *const c_char,
    inviter_user_id: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(inviter_user_id, "inviter_user_id")?;
        let this = unsafe { handle(client) }?;
        let code = unsafe { text(code, "code") }?;
        let inviter = wait(this.client.redeem_invite(code))?;
        unsafe { *out = owned(inviter.to_string()) };
        Ok(())
    })
}

/// Revokes one of the user's invites.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_revoke_invite(
    client: *const PushClient,
    code: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let code = unsafe { text(code, "code") }?;
        wait(this.client.revoke_invite(code))
    })
}

/// The profiles among `count` user ids the user may see, as a JSON array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_profiles(
    client: *const PushClient,
    user_ids: *const *const c_char,
    count: usize,
    profiles_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(profiles_json, "profiles_json")?;
        let this = unsafe { handle(client) }?;
        let ids = if count == 0 {
            &[][..]
        } else if user_ids.is_null() {
            return Err(SdkError::Invalid("user_ids is NULL".to_owned()));
        } else {
            unsafe { std::slice::from_raw_parts(user_ids, count) }
        };
        let ids = ids
            .iter()
            .map(|id| unsafe { user_id(*id) })
            .collect::<Result<Vec<_>, _>>()?;
        let profiles = wait(this.client.profiles(ids))?;
        put_json(out, &json::list(&profiles, json::profile));
        Ok(())
    })
}

/// The profile with a handle, as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_lookup(
    client: *const PushClient,
    handle_name: *const c_char,
    profile_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(profile_json, "profile_json")?;
        let this = unsafe { handle(client) }?;
        let handle_name = unsafe { text(handle_name, "handle_name") }?;
        let profile = wait(this.client.lookup(handle_name))?;
        put_json(out, &json::profile(&profile));
        Ok(())
    })
}

/// Changes the user's display name, handle or both; NULL leaves one as it is (at least
/// one must be given). `*profile_json` gets the profile after the change.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_update_profile(
    client: *const PushClient,
    display_name: *const c_char,
    handle_name: *const c_char,
    profile_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(profile_json, "profile_json")?;
        let this = unsafe { handle(client) }?;
        let display_name = if display_name.is_null() {
            None
        } else {
            Some(unsafe { text(display_name, "display_name") }?)
        };
        let handle_name = if handle_name.is_null() {
            None
        } else {
            Some(unsafe { text(handle_name, "handle_name") }?)
        };
        let profile = wait(this.client.update_profile(display_name, handle_name))?;
        put_json(out, &json::profile(&profile));
        Ok(())
    })
}

/// The account's devices, as a JSON array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_devices(
    client: *const PushClient,
    devices_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(devices_json, "devices_json")?;
        let this = unsafe { handle(client) }?;
        let devices = wait(this.client.devices())?;
        put_json(out, &json::list(&devices, json::device));
        Ok(())
    })
}

/// Revokes one of the account's devices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_revoke_device(
    client: *const PushClient,
    device_id: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let device: DeviceId = json::id(unsafe { text(device_id, "device_id") }?, "device_id")?;
        wait(this.client.revoke_device(device))
    })
}
