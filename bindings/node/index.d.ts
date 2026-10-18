// Types of push-sdk for Node.js (index.js). docs/sdk-bindings.md describes every call.
// Ids are UUID strings; times are microseconds since the Unix epoch.

/** The servers and the database of one app profile. */
export interface Config {
  /** auth-svc's public URL: `https://push.example`. */
  authUrl: string;
  /** The gateway's WebSocket: `wss://push.example/v1/socket`. */
  socketUrl: string;
  /** The SQLite file of this profile; `:memory:` keeps nothing. */
  databasePath: string;
  /** The `Host` header, when the URLs name an address rather than the virtual host. */
  host?: string;
  /** How long a request waits for the connection, and then for its answer (15 000). */
  requestTimeoutMs?: number;
  /**
   * Allows `http://` and `ws://` URLs beyond this machine (loopback, `localhost`,
   * `*.localhost`, `*.local`, `*.test`), which send passwords, tokens and messages in
   * plaintext. Development clusters only (false).
   */
  allowInsecureTransport?: boolean;
  /** 32 bytes that encrypt the whole database; never stored or logged by the SDK. */
  databaseKey?: Uint8Array;
}

/**
 * The app's secret store: a few small values under `SECRET_KEYS`. Methods may return
 * promises. Loaded once when the client opens, then written through. A `save` or `delete`
 * that has not answered within 30 s fails the call with `secrets`; if the store carries it
 * out after all, the SDK follows it (a newer write still wins), and a late failure changes
 * nothing.
 *
 * The methods must not call the client (`account()`, `signIn()`, anything): the SDK waits
 * for the store while the client's call waits for the JavaScript thread, so the two
 * deadlock until the 30 s end.
 */
export interface SecretStore {
  load(key: string): Uint8Array | null | undefined | Promise<Uint8Array | null | undefined>;
  save(key: string, value: Uint8Array): void | Promise<void>;
  delete(key: string): void | Promise<void>;
}

export type Platform = 'ios' | 'android' | 'web' | 'desktop' | 'other';

export interface Account {
  userId: string;
  deviceId: string;
}

export type ConnectionState =
  | { state: 'offline' | 'connecting' | 'connected' | 'signedOut' | 'deviceRemoved' | 'openedElsewhere' }
  | { state: 'reconnecting'; attempt: number; retryInMs: number };

export interface Message {
  conversationId: string;
  /** Chosen by the sending device; the handle for edit, delete and cancel. */
  clientMsgId: string;
  messageId: string | null;
  seq: number | null;
  senderId: string;
  kind: 'text' | 'system';
  /** null once deleted. */
  body: string | null;
  clientTs: number | null;
  serverTs: number | null;
  editedAt: number | null;
  deleted: boolean;
  status: 'pending' | 'sent' | 'delivered' | 'read' | 'received';
}

export interface Conversation {
  id: string;
  kind: 'direct' | 'group' | 'unknown';
  peer: string | null;
  title: string | null;
  lastSeq: number;
  readSeq: number;
  unread: number;
  peerDeliveredSeq: number;
  peerReadSeq: number;
  updatedAt: number;
}

export interface Contact {
  userId: string;
  state: 'none' | 'requestedOut' | 'requestedIn' | 'accepted' | 'blocked';
}

export interface Invite {
  code: string;
  expiresAt: number;
  maxUses: number;
  uses: number;
}

export interface Profile {
  userId: string;
  handle: string;
  displayName: string;
}

export interface Device {
  deviceId: string;
  name: string;
  platform: string;
  createdAt: string;
  lastSeenAt: string | null;
  current: boolean;
}

export interface ErrorDetail {
  code: string;
  message: string;
  retryable: boolean;
  retryAfterS: number | null;
}

export type PushEvent =
  | ({ type: 'connectionStateChanged' } & ConnectionState)
  | { type: 'messageAdded' | 'messageSent' | 'messageEdited' | 'messageDeleted'; message: Message }
  | {
      type: 'sendFailed';
      conversationId: string;
      clientMsgId: string;
      kind: 'send' | 'edit' | 'delete';
      error: ErrorDetail;
    }
  | { type: 'receiptChanged'; conversationId: string; peerDeliveredSeq: number; peerReadSeq: number }
  | { type: 'conversationChanged'; conversation: Conversation }
  | { type: 'contactChanged'; contact: Contact }
  | { type: 'signedOut' | 'deviceRemoved' | 'openedElsewhere' | 'eventsLost' };

/** A failed call; `code` is the SDK's stable error code, as in Rust and C. */
export class PushError extends Error {
  constructor(detail: { code: string; message: string; retryable?: boolean; retryAfterS?: number | null });
  readonly name: 'PushError';
  readonly code: string;
  readonly retryable: boolean;
  readonly retryAfterS: number | null;
}

export class Client {
  private constructor();
  static open(config: Config, secretStore: SecretStore): Promise<Client>;
  /**
   * Delivers every event, in order, to `listener`; one listener per client. Throws `closed`
   * once the client is closed. A listener keeps
   * Node running until `close()`. The connection alone does not: without an event listener or
   * another pending call, Node may exit after `connect()`, so an app keeps a listener or a
   * pending call.
   */
  onEvent(listener: (event: PushEvent) => void): void;
  /**
   * Stops the connection and closes the database. Calls still pending reject with `closed`
   * and their work is aborted, so the database is closed when this resolves. Rejects with
   * `timeout` if that work does not end within 5 s (the database closes once it does).
   * The whole close takes at most about 15 s: 5 s for the calls, 5 s for other holders of the
   * client, 5 s for the runtime to end. Later
   * calls fail with `closed`; calling `close()` again returns the same promise.
   */
  close(): Promise<void>;

  signUp(email: string, password: string, handle: string, displayName: string): Promise<void>;
  resendVerification(email: string): Promise<void>;
  forgotPassword(email: string): Promise<void>;
  signIn(email: string, password: string, deviceName: string, platform: Platform): Promise<Account>;
  /**
   * Signs in with a Microsoft Entra ID access token for auth-svc's API, obtained by the app
   * with MSAL (scope `api://<api-client-id>/access_as_user`). `handle` names the account on
   * its first sign-in only. Rejects with `entra_token_invalid` or `entra_unavailable`.
   */
  signInWithEntra(accessToken: string, handle: string | null, deviceName: string, platform: Platform): Promise<Account>;
  account(): Account | null;
  signOut(): Promise<void>;

  connect(): void;
  disconnect(): void;
  connectionState(): ConnectionState;
  waitConnected(timeoutMs: number): Promise<void>;
  changeDatabaseKey(databaseKey: Uint8Array | null): Promise<void>;

  conversations(): Promise<Conversation[]>;
  conversation(conversationId: string): Promise<Conversation | null>;
  messages(conversationId: string, beforeSeq: number | null, limit: number): Promise<Message[]>;
  contacts(): Promise<Contact[]>;

  send(conversationId: string, text: string): Promise<Message>;
  edit(conversationId: string, clientMsgId: string, text: string): Promise<string>;
  delete(conversationId: string, clientMsgId: string): Promise<string>;
  cancel(conversationId: string, clientMsgId: string): Promise<boolean>;
  markRead(conversationId: string, seq: number): Promise<void>;

  listConversations(): Promise<Conversation[]>;
  fetchHistory(conversationId: string, beforeSeq: number | null, limit: number): Promise<Message[]>;
  openDirect(userId: string): Promise<Conversation>;
  listContacts(): Promise<Contact[]>;
  requestContact(userId: string): Promise<Contact>;
  acceptContact(userId: string): Promise<Contact>;
  declineContact(userId: string): Promise<Contact>;
  block(userId: string): Promise<Contact>;
  unblock(userId: string): Promise<Contact>;
  createInvite(expiresInS: number, maxUses: number): Promise<Invite>;
  redeemInvite(code: string): Promise<string>;
  revokeInvite(code: string): Promise<void>;
  profiles(userIds: string[]): Promise<Profile[]>;
  lookup(handle: string): Promise<Profile>;
  /**
   * Changes the user's display name, handle or both (null leaves one as it is; at least one).
   * Rejects with `handle_taken` when someone else holds the handle, `rate_limited` after 5
   * handle changes a day.
   */
  updateProfile(displayName: string | null, handle: string | null): Promise<Profile>;
  devices(): Promise<Device[]>;
  revokeDevice(deviceId: string): Promise<void>;
}

export function deriveDatabaseKey(secret: Uint8Array | string, salt: Uint8Array | string): Promise<Uint8Array>;
export function newSalt(): Uint8Array;
export const SECRET_KEYS: readonly string[];
