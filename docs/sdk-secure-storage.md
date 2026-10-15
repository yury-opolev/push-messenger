# Keeping a push-sdk app's secrets safe

`push-sdk` keeps two kinds of things on the device, and the app decides how each is
protected:

- **The device's credentials** — its access and refresh tokens, its device id and a
  registration in progress. The SDK never writes them anywhere itself: it hands them to a
  `SecretStore` the app provides. Anyone who copies them can act as the user on that
  device until the device is revoked.
- **The local database** — conversations, message history, contacts and the outbox of
  unsent messages, in one SQLite file. It is plain unless the app opens the client with a
  `DatabaseKey`; then the whole file is encrypted.

The SDK ships no keychain integration (owner decision): every platform has its own, and
the app already knows which one it runs on. This guide shows how to connect the SDK to the
platform's secure storage, what never to do, and how to derive and change the database
key.

## The secret store

```rust
pub trait SecretStore: Send + Sync {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError>;
    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError>;
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}
```

The SDK uses three keys, listed in `push_sdk::SECRET_KEYS`: `push.session` (the tokens),
`push.device` (the device id, kept after a password reset so the same device signs in
again) and `push.registration` (the id of a device registration not answered yet).

Rules for an implementation:

- **The values are opaque bytes.** Store them exactly as given and return them unchanged;
  never parse, trim or re-encode them. If the platform API only takes text, encode with
  base64 and decode on the way back. Each value is small (under 2 KiB).
- **`load` of a missing key is `Ok(None)`, not an error;** `delete` of a missing key
  succeeds. An error means the store could not answer (locked, unavailable): the SDK then
  fails the call and does not guess.
- **Error messages never contain a value.** Map the platform's error to a fixed
  description; some platform errors carry the data they failed on.
- **The calls block and come from SDK threads.** Keep them quick; never call back into
  the client from inside them. The store must be safe to call from several threads.
- **Make the items device-only.** They must not sync to other devices or be restored onto
  another one: a copy of the credentials on a second machine makes two apps share one
  device, and the gateway lets only one of them stay connected (the other reports
  `OpenedElsewhere`).
- **Overwrite buffers you made.** Where the language allows, clear temporary copies of a
  value after use.

### Never do this

- Never write the values to a plain file, a database, `SharedPreferences`,
  `UserDefaults`, `localStorage`, the registry or an environment variable. Any program
  running as the same user — or anyone with a backup — can read them and sign in as the
  user.
- Never log them, put them in crash reports or analytics, or show them in a UI.
- Never fall back to a file when the keychain is unavailable (a Linux machine without a
  Secret Service, say). Fail, and tell the user why.
- Never keep a PIN or password the user entered once the key is derived.

### Desktop: Windows, macOS and Linux from Rust

The [`keyring`](https://crates.io/crates/keyring) crate (version 4, with its default `v1`
feature) reaches the Windows Credential Manager, the macOS Keychain and the Linux Secret
Service (GNOME Keyring, KWallet) behind one API, choosing the store for the platform it
runs on. An app that wants to pick its stores itself uses `keyring-core` and the store
crates directly; the calls are the same.

```rust
use keyring::v1::{Entry, Error};
use push_sdk::{SecretError, SecretStore};

/// The operating system's credential store, under one service name per app profile.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new(service: &str) -> Self {
        Self { service: service.to_owned() }
    }

    fn entry(&self, key: &str) -> Result<Entry, SecretError> {
        Entry::new(&self.service, key).map_err(failed)
    }
}

impl SecretStore for KeyringStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        match self.entry(key)?.get_secret() {
            Ok(value) => Ok(Some(value)),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(failed(error)),
        }
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        self.entry(key)?.set_secret(value).map_err(failed)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(error) => Err(failed(error)),
        }
    }
}

/// A fixed description: some keyring errors (`BadEncoding`, `BadDataFormat`) carry the
/// bytes they failed on.
fn failed(error: Error) -> SecretError {
    let what = match error {
        Error::NoStorageAccess(_) => "the credential store is locked or unavailable",
        Error::PlatformFailure(_) => "the credential store failed",
        _ => "the credential store refused the request",
    };
    SecretError(what.to_owned())
}
```

Platform notes:

- **Windows Credential Manager** keeps generic credentials per user, protected with DPAPI.
  A credential's blob holds at most 2,560 bytes, which the SDK's values fit. Written with
  `CRED_PERSIST_LOCAL_MACHINE`, an item stays on this machine and does not roam with the
  user's profile; check the persistence your store uses.
- **macOS Keychain**: items go to the user's login keychain; a signed app is the only one
  allowed to read them without asking the user.
- **Linux libsecret**: the Secret Service must be running and unlocked (it is in a normal
  desktop session). On a headless machine there is none: fail rather than store elsewhere.

### iOS and macOS apps in Swift

Through the C or Node binding (Plan 10) the three calls become callbacks. Back them with
generic-password items, one per key, readable only on this device after its first unlock
(so the app can reconnect in the background) and never synced or restored elsewhere:

```swift
let service = "com.example.chat.push"

func save(_ key: String, _ value: Data) -> Bool {
    let item: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: service,
        kSecAttrAccount as String: key,
    ]
    SecItemDelete(item as CFDictionary)
    var add = item
    add[kSecValueData as String] = value
    add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
    return SecItemAdd(add as CFDictionary, nil) == errSecSuccess
}

func load(_ key: String) -> (found: Bool, value: Data?, ok: Bool) {
    let query: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: service,
        kSecAttrAccount as String: key,
        kSecReturnData as String: true,
        kSecMatchLimit as String: kSecMatchLimitOne,
    ]
    var result: AnyObject?
    switch SecItemCopyMatching(query as CFDictionary, &result) {
    case errSecSuccess: return (true, result as? Data, true)
    case errSecItemNotFound: return (false, nil, true)
    default: return (false, nil, false)
    }
}

func delete(_ key: String) -> Bool {
    let item: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: service,
        kSecAttrAccount as String: key,
    ]
    let status = SecItemDelete(item as CFDictionary)
    return status == errSecSuccess || status == errSecItemNotFound
}
```

### Android in Kotlin

The Android Keystore holds keys, not arbitrary data: keep an AES-256-GCM key in it (it
never leaves the secure hardware where there is one) and store the values sealed with it
in the app's private preferences. Exclude that file from backups
(`android:dataExtractionRules` / `android:fullBackupContent`), so a sealed value never
reaches a device whose Keystore cannot open it.

```kotlin
private const val ALIAS = "push-sdk"
private val prefs = context.getSharedPreferences("push-sdk-secrets", Context.MODE_PRIVATE)

private fun keystoreKey(): SecretKey {
    val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (store.getKey(ALIAS, null) as? SecretKey)?.let { return it }
    val spec = KeyGenParameterSpec.Builder(
        ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
    )
        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
        .setKeySize(256)
        .build()
    return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        .apply { init(spec) }
        .generateKey()
}

fun save(key: String, value: ByteArray) {
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(Cipher.ENCRYPT_MODE, keystoreKey())
    val sealed = cipher.iv + cipher.doFinal(value) // the 12-byte IV first
    check(prefs.edit().putString(key, Base64.encodeToString(sealed, Base64.NO_WRAP)).commit())
}

fun load(key: String): ByteArray? {
    val text = prefs.getString(key, null) ?: return null
    val sealed = Base64.decode(text, Base64.NO_WRAP)
    val cipher = Cipher.getInstance("AES/GCM/NoPadding")
    cipher.init(Cipher.DECRYPT_MODE, keystoreKey(), GCMParameterSpec(128, sealed, 0, 12))
    return cipher.doFinal(sealed, 12, sealed.size - 12)
}

fun delete(key: String) {
    check(prefs.edit().remove(key).commit())
}
```

## The database key

Opened with a key, the client encrypts the whole SQLite file — every page of the history,
the contacts, the outbox and the write-ahead log — with SQLite3 Multiple Ciphers
(ChaCha20-Poly1305, its default). Without the key nobody can read the user's messages from
the file: not another program, not a backup, not the app itself. The SDK never stores the
key; the app provides it every time it opens the client.

```rust
let mut config = Config::new(auth_url, socket_url, &database_path);
config.database_key = Some(DatabaseKey::derive(pin.as_bytes(), &salt)?);
let (client, events) = match Client::open(config, secrets) {
    Err(SdkError::WrongKey) => return ask_for_the_pin_again(),
    opened => opened?,
};
```

Encryption is optional: without `Config::database_key` the file is plain.

`SdkError::WrongKey` means the key does not open the file — a wrong PIN, a database that
has a key opened without one, or a plain database opened with one. Nothing is written
before the key is checked, so a wrong key never damages the data.

`SdkError::EncryptionUnavailable` (code `encryption_unavailable`, fatal: do not retry)
means a key was given but the SQLite linked into the program cannot encrypt, because
another crate brought a plain SQLite. The SDK refuses to open rather than write plaintext
the user believes is encrypted. Never fall back to opening without the key.

**`Client::open` is synchronous.** It opens the file, checks the key and migrates the
schema, so it blocks. Call it off the UI thread: on a blocking thread
(`tokio::task::spawn_blocking`) in an async app, and from a worker thread in a binding
(an `openAsync` in Node, a background queue on iOS, a coroutine on `Dispatchers.IO`).

When the app is done with the file and needs to open it again, rekey it or delete it, wait
for `Client::close()`: it stops the connection, ends the runtime and closes the database
before it returns. Dropping the client does the same without waiting.

### Deriving the key from a PIN or a password

`DatabaseKey::derive(secret, salt)` runs Argon2id (19 MiB, 2 passes, 1 lane), which turns
what the user typed into a 32-byte key and makes every guess cost tens of milliseconds.

1. The first time, make a salt with `DatabaseKey::new_salt()` and keep it next to the
   database: in the app's preferences, or a small file beside it. The salt is not
   secret, but it must not be lost — without it the same PIN gives another key.
2. Each time the app starts, ask for the PIN, derive the key with the stored salt and open
   the client. Derive it off the UI thread.
3. Forget the PIN and the key as soon as the client is open.

**The helper's parameters are a floor, not a guarantee.** Argon2id at 19 MiB and 2 passes
is what OWASP recommends for a minimum; it slows each guess down but cannot make a weak
secret strong. Someone holding a copy of the file guesses offline, on hardware of their
choice, with no lockout and no rate limit. What the key is worth is the entropy of what the
user types. As a guide, assuming an attacker who tries a million guesses a second (a
generous allowance for a determined one at these parameters):

| What the user types | Entropy | Time to try all |
|---|---|---|
| 4-digit PIN | 13 bits | a few milliseconds |
| 6-digit PIN | 20 bits | about a second |
| 8-digit PIN | 27 bits | about 2 minutes |
| 8 random lowercase letters | 38 bits | about 2 days |
| 10 random letters and digits | 60 bits | about 27 millennia |
| 4 random words from a 7,776-word list | 51 bits | about 120 years |
| 5 random words | 65 bits | about 900 millennia |

Aim for at least 50 bits and prefer 60 or more, which means a passphrase of random words,
not a PIN. A name, a date or a known phrase has far less entropy than its length suggests.
If you must accept less, raise the cost of a guess above the helper's parameters (derive
the key yourself with more memory and passes, and pass it to `DatabaseKey::from_bytes`),
or let the platform add what a copy of the file lacks: generate a random key once
(`DatabaseKey::from_bytes` with 32 random bytes), keep it sealed by a hardware key that
requires the user's authentication (an Android Keystore key with
`setUserAuthenticationRequired`, a Secure Enclave key with an access control), and unseal
it when the user unlocks. The hardware then limits the guesses; the file alone is
useless. Even so, a short PIN protects only as long as that hardware does.

Bytes from elsewhere — a hardware-held key, a key from your own backend — go in with
`DatabaseKey::from_bytes` (exactly 32 bytes).

### Changing, adding and removing the key

`Client::change_database_key(Some(&new_key))` re-encrypts the file in one transaction (a
crash leaves the old key or the new one, never a mix); `Some` on a plain database encrypts
it, `None` decrypts it. When the user changes the PIN:

1. derive the new key with a **new** salt;
2. save the new salt under a second name ("pending"), keeping the old one;
3. call `change_database_key`;
4. replace the old salt with the pending one.

`change_database_key` is async and waits for the work to finish; the client does not keep
the key, so the next `Client::open` needs the new one in `Config::database_key`.

**After a crash during `change_database_key`**, the file has either the old key or the new
one (the change is one transaction), and the app cannot tell which. On `WrongKey`, try the
other key before telling the user their PIN is wrong: the old salt with the old PIN, then
the pending salt with the new one. If the app stopped between steps 2 and 4, the same
applies: try the old salt, then the pending one, and finish step 4 for whichever opened.

**Adding a key to a database that is already in use leaves old plaintext behind.**
`change_database_key(Some(&key))` on a plain database encrypts the file, but SQLite's
deleted pages, and the write-ahead log or rollback journal a plain database used before,
may still hold plaintext copies of earlier content, and it can remain in free space of the
disk the file system has not overwritten. Choose encryption at the first open, before any
history is stored. For a database that already holds history, the safe course is to
`close()` the client, delete the file and its `-wal`, `-shm` and `-journal` files, open a
new encrypted database, sign in again and let the history sync from the server.

### A forgotten key

It cannot be recovered: that is the point. `close()` the client, delete the database file
(and its `-wal`, `-shm` and `-journal` files), sign in again, and the client starts empty;
history comes back from the server as the user scrolls (`fetch_history`). Messages that
were waiting in the outbox are lost.

### What the key does not cover

- While the client is open the data is in the app's memory, readable by anyone who can
  read that memory.
- The file's size and modification times stay visible, so an observer can tell roughly how
  much the app stores and when it changed.
- The credentials are in the secret store, not the database: protect them as above.

## Signing out and remote wipe

The SDK never erases local data on the server's word. A sign-out by the server (close code
4001, after a password reset) and a removed device (4003) keep the history and the unsent
messages, so the same person finds them when signing in again; 4007 means the data was
"opened elsewhere" and also keeps everything. The data is erased only when a different
person signs in on that database, or when the user signs out in the app:

- `Client::sign_out` first stops the connection (waiting up to 5 seconds), then always
  wipes: the credentials, the device and the database with its outbox, also offline, and
  even if the keychain delete fails (the first error is returned). Only then does it ask
  auth-svc to revoke the device. If the connection does not stop in time it returns
  `connection_busy` (retryable) and erases nothing: call it again.
- `Client::sign_in` also stops any existing connection first, so a connection of an earlier
  user never writes into the new user's data. Call `connect()` again afterwards.

Error codes an app meets here, besides `wrong_key`: `encryption_unavailable` (fatal),
`timeout` and `unexpected_answer` (neither retryable: the request may have been carried
out, or the server speaks another protocol version) and `connection_busy` (retryable).

A remote wipe is not a security feature and the SDK does not offer one: a thief can keep
the device offline, so a wipe that waits for a connection protects nothing. What protects
the data on a lost device is the device's own lock and, for the history, the database key.
