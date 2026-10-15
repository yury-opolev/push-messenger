'use strict';

/**
 * push-sdk for Node.js: the client core of push-messenger (spec §8), over the Rust SDK.
 *
 * - Every call that talks to a server or reads or writes the database returns a promise; the
 *   JavaScript thread never waits for the SDK.
 * - Failures are `PushError`s with the SDK's stable `code` (the same as in Rust and C),
 *   `retryable` and, when rate limited, `retryAfterS`.
 * - Events come, in order, to the one listener given to `onEvent`.
 * - Credentials live only in the app's secret store: an object with `load`, `save` and
 *   `delete` (each may return a promise). docs/sdk-secure-storage.md shows how to back it with
 *   the platform's keychain. The SDK loads the values once when the client opens and writes
 *   through to the store, so the store must not change behind its back while it is open.
 *   A `save` or `delete` that has not answered within 30 s fails the call with `secrets`;
 *   if the store carries it out after all, the SDK follows it (a newer write still wins), so
 *   its view of the store stays right; a late failure changes nothing. The store's methods
 *   must not call the client (`account()`, `signIn()`, anything): the SDK waits for the
 *   store while the client's call waits for the JavaScript thread, and the two deadlock
 *   until the 30 s end.
 * - Close the client when done: while it has a listener, Node keeps running. The
 *   connection itself does not keep Node running: without an event listener or another
 *   pending call, Node may exit after `connect()`. An app keeps a listener (`onEvent`) or a
 *   pending call.
 * - `close()` rejects the calls still pending with `closed` and aborts their work, so the
 *   database is closed when it resolves. It rejects with `timeout` if that work does not end
 *   within 5 s; the database closes once it does. The whole close takes at most about 15 s
 *   (5 s for the calls, 5 s for other holders of the client, 5 s for the runtime to end).
 *   Calling it again gives the same promise.
 *
 * docs/sdk-bindings.md describes every call and the shapes of the data.
 */

const path = require('node:path');

const native = require(path.join(__dirname, 'push_sdk.node'));

/** A failed call. `code` is the SDK's stable error code. */
class PushError extends Error {
  constructor(detail) {
    super(detail.message);
    this.name = 'PushError';
    this.code = detail.code;
    this.retryable = Boolean(detail.retryable);
    this.retryAfterS = detail.retryAfterS == null ? null : detail.retryAfterS;
  }
}

/** The native layer's error as a PushError; an argument it refused is `invalid`. */
function toPushError(error) {
  let detail = null;
  try {
    detail = JSON.parse(error.message);
  } catch {
    detail = null;
  }
  if (detail && typeof detail.code === 'string') {
    return new PushError(detail);
  }
  return new PushError({ code: 'invalid', message: String(error.message), retryable: false });
}

async function settled(call) {
  try {
    return await call();
  } catch (error) {
    throw toPushError(error);
  }
}

function now(call) {
  try {
    return call();
  } catch (error) {
    throw toPushError(error);
  }
}

/**
 * The function the native layer calls for the app's store: it runs the store's method and
 * answers with `completeSecretCall`, exactly once, whether the method returns, resolves,
 * throws or rejects.
 */
function secretBridge(store) {
  const operations = {
    load: (key) => store.load(key),
    save: (key, value) => store.save(key, value),
    delete: (key) => store.delete(key),
  };
  return (id, operation, key, value) => {
    Promise.resolve()
      .then(() => operations[operation](key, value))
      .then((result) => (operation === 'load' ? asBytes(result) : null))
      .then(
        (loaded) => native.completeSecretCall(id, null, loaded),
        (error) => native.completeSecretCall(id, String((error && error.message) || error), null),
      )
      // Whatever else goes wrong still answers the waiting call, and is never an unhandled
      // rejection (which ends Node).
      .catch((error) => {
        try {
          native.completeSecretCall(id, String((error && error.message) || error), null);
        } catch {
          // The call is answered already or Node is shutting down.
        }
      });
  };
}

/** What `load` returned as a Buffer: null for nothing; anything but bytes is an error. */
function asBytes(result) {
  if (result == null) {
    return null;
  }
  if (result instanceof Uint8Array) {
    return Buffer.from(result.buffer, result.byteOffset, result.byteLength);
  }
  if (result instanceof ArrayBuffer) {
    return Buffer.from(result);
  }
  throw new TypeError(`load returned ${typeof result}, not bytes`);
}

function checkStore(store) {
  for (const name of ['load', 'save', 'delete']) {
    if (!store || typeof store[name] !== 'function') {
      throw new PushError({
        code: 'invalid',
        message: `the secret store has no ${name} function`,
        retryable: false,
      });
    }
  }
}

class Client {
  #native;
  #closing = null;

  /** Use `Client.open`. */
  constructor(nativeClient) {
    this.#native = nativeClient;
  }

  /**
   * Opens a client: loads the credentials from `secretStore`, then opens the database (off
   * the JavaScript thread: a database key is checked, the schema migrated). It does not
   * connect until `connect()`.
   */
  static async open(config, secretStore) {
    checkStore(secretStore);
    const { databaseKey, ...settings } = config || {};
    const key = databaseKey == null ? null : Buffer.from(databaseKey);
    const opened = await settled(() => native.open(settings, key, secretBridge(secretStore)));
    return new Client(opened);
  }

  /** Sends every event to `listener`, in order. Once per client. */
  onEvent(listener) {
    if (typeof listener !== 'function') {
      throw new PushError({ code: 'invalid', message: 'the listener is not a function', retryable: false });
    }
    now(() =>
      this.#native.onEvent((event) => {
        try {
          listener(event);
        } catch (error) {
          // A listener's own failure is the app's: reported like any uncaught exception.
          queueMicrotask(() => {
            throw error;
          });
        }
      }),
    );
  }

  /** Stops the connection and closes the database; later calls fail with `closed`. Every caller gets the same promise. */
  close() {
    this.#closing ??= settled(() => this.#native.close());
    return this.#closing;
  }

  // ----- account -----

  signUp(email, password, handle, displayName) {
    return settled(() => this.#native.signUp(email, password, handle, displayName));
  }

  resendVerification(email) {
    return settled(() => this.#native.resendVerification(email));
  }

  forgotPassword(email) {
    return settled(() => this.#native.forgotPassword(email));
  }

  signIn(email, password, deviceName, platform) {
    return settled(() => this.#native.signIn(email, password, deviceName, platform));
  }

  /** The signed-in account, or null. Answered at once. */
  account() {
    return now(() => this.#native.account());
  }

  signOut() {
    return settled(() => this.#native.signOut());
  }

  // ----- connection -----

  /** Starts connecting and keeps reconnecting until `disconnect()`. Returns at once. */
  connect() {
    now(() => this.#native.connect());
  }

  disconnect() {
    now(() => this.#native.disconnect());
  }

  /** The connection's state now. */
  connectionState() {
    return now(() => this.#native.connectionState());
  }

  waitConnected(timeoutMs) {
    return settled(() => this.#native.waitConnected(timeoutMs));
  }

  changeDatabaseKey(databaseKey) {
    const key = databaseKey == null ? null : Buffer.from(databaseKey);
    return settled(() => this.#native.changeDatabaseKey(key));
  }

  // ----- local data -----

  /** Every conversation this device knows, most recently changed first. */
  conversations() {
    return settled(() => this.#native.conversations());
  }

  /** One conversation, or null. */
  conversation(conversationId) {
    return settled(() => this.#native.conversation(conversationId));
  }

  /**
   * Up to `limit` stored messages before `beforeSeq` (null: the newest, then those still in
   * the outbox), oldest first.
   */
  messages(conversationId, beforeSeq, limit) {
    return settled(() => this.#native.messages(conversationId, beforeSeq ?? null, limit));
  }

  contacts() {
    return settled(() => this.#native.contacts());
  }

  // ----- sending -----

  /** Queues a message; resolves to the local echo, whose `clientMsgId` is its handle. */
  send(conversationId, text) {
    return settled(() => this.#native.send(conversationId, text));
  }

  /** Resolves to the change's own id. */
  edit(conversationId, clientMsgId, text) {
    return settled(() => this.#native.edit(conversationId, clientMsgId, text));
  }

  /** Resolves to the change's own id. */
  delete(conversationId, clientMsgId) {
    return settled(() => this.#native.delete(conversationId, clientMsgId));
  }

  /** Takes back a message never sent; resolves to whether it was. */
  cancel(conversationId, clientMsgId) {
    return settled(() => this.#native.cancel(conversationId, clientMsgId));
  }

  markRead(conversationId, seq) {
    return settled(() => this.#native.markRead(conversationId, seq));
  }

  // ----- requests -----

  listConversations() {
    return settled(() => this.#native.listConversations());
  }

  fetchHistory(conversationId, beforeSeq, limit) {
    return settled(() => this.#native.fetchHistory(conversationId, beforeSeq ?? null, limit));
  }

  openDirect(userId) {
    return settled(() => this.#native.openDirect(userId));
  }

  listContacts() {
    return settled(() => this.#native.listContacts());
  }

  requestContact(userId) {
    return settled(() => this.#native.changeContact('request', userId));
  }

  acceptContact(userId) {
    return settled(() => this.#native.changeContact('accept', userId));
  }

  declineContact(userId) {
    return settled(() => this.#native.changeContact('decline', userId));
  }

  block(userId) {
    return settled(() => this.#native.changeContact('block', userId));
  }

  unblock(userId) {
    return settled(() => this.#native.changeContact('unblock', userId));
  }

  createInvite(expiresInS, maxUses) {
    return settled(() => this.#native.createInvite(expiresInS, maxUses));
  }

  /** Resolves to the inviter's user id. */
  redeemInvite(code) {
    return settled(() => this.#native.redeemInvite(code));
  }

  revokeInvite(code) {
    return settled(() => this.#native.revokeInvite(code));
  }

  profiles(userIds) {
    return settled(() => this.#native.profiles(userIds));
  }

  lookup(handle) {
    return settled(() => this.#native.lookup(handle));
  }

  devices() {
    return settled(() => this.#native.devices());
  }

  revokeDevice(deviceId) {
    return settled(() => this.#native.revokeDevice(deviceId));
  }
}

/** Derives a 32-byte database key from what the user knows and a salt the app keeps. */
function deriveDatabaseKey(secret, salt) {
  const asBytes = (value) => (typeof value === 'string' ? Buffer.from(value, 'utf8') : Buffer.from(value));
  return settled(() => native.deriveDatabaseKey(asBytes(secret), asBytes(salt)));
}

/** A new random salt for `deriveDatabaseKey`, to keep next to the database. */
function newSalt() {
  return native.newSalt();
}

/** The keys the SDK keeps in the secret store. */
const SECRET_KEYS = Object.freeze(native.secretKeys());

module.exports = { Client, PushError, deriveDatabaseKey, newSalt, SECRET_KEYS };
