# Deleted accounts as tombstones, `peer_deleted`, and hiding direct conversations: design

Change request (2026-11-06, "deleted accounts in profiles, and hiding a direct
conversation"): when an owner deletes an agent (or any account is disabled), peers keep a
direct conversation with a bare user id, `GET /v1/users?ids=…` silently omits the account,
and a direct conversation cannot be left or hidden. A follow-up ("agent connection policy +
owner view") builds on the tombstone and the deleted-peer flag, so both are generic.

## Decisions

| # | Decision |
|---|---|
| D1 | **Tombstones in `GET /v1/users`.** An id that message-svc's `Directory.VisibleUsers` already lets the caller see (§5.5: self, shared conversation, incoming request, accepted contact, blocked with `sees_peer`) and whose verified account is disabled (`disabled_at` set) is answered as a tombstone: `{user_id, kind, display_name, handle: "", owner_user_id: null, deleted: true, state: "deleted"}`. `display_name` is `"Deleted agent"` for `kind: "agent"` and `"Deleted account"` otherwise. Every other id stays omitted (unknown, never verified, or not visible). Live profiles gain `deleted: false, state: "active"`. `GET /v1/users/lookup` and `/v1/users/me` are unchanged (a disabled account is still not found). |
| D2 | **Visibility for deleted accounts is unchanged.** It is decided by message-svc from the contact and membership rows, which deleting an account does not remove, so exactly the ids that were visible before the deletion get a tombstone and nothing new is revealed. auth-svc never widens the set: it answers tombstones only for ids the Directory returned. |
| D3 | **`handle` is the empty string, not absent.** Older SDKs parse `handle` as a required string; an empty string keeps their whole batch parseable (they show "Deleted agent"), and new clients use `deleted`. There is no avatar field, so nothing else is withheld; the owner is withheld (`owner_user_id: null`). |
| D4 | **message-svc learns deletions.** auth-svc writes a new outbox event `user.deleted {user_id}` in the transaction that disables the account; the worker relays it to a new `Accounts.UserDeleted(AccountUser)` RPC. message-svc records the user in `messaging.deleted_user` (idempotent, order-independent: it also records the user like `DeviceRegistered`) and publishes `ConversationFlags` to the other member of each of the user's direct conversations. auth migration 0010 allows the kind and backfills one event per already-disabled verified account. |
| D5 | **`peer_deleted`** on `ConversationSummary` (field 13): set for a direct conversation whose other member is recorded deleted. Always false for groups. |
| D6 | **Hide / unhide.** New requests `HideConversation{conversation_id}` (22) and `UnhideConversation{conversation_id}` (23), answered `Done`, for direct conversations only (a group is `INVALID`; unknown `CONVERSATION_NOT_FOUND`; not a member `NOT_MEMBER`). Idempotent. Per user: the peer's view is untouched. Stored in `messaging.conversation_view(conversation_id, user_id, hidden, changed_at)`. |
| D7 | **Listing filter.** `ListConversations.include_hidden` (field 2, default false): hidden conversations are left out unless set. `ConversationSummary.hidden` (field 12) says whether it is hidden. The SDK always lists with `include_hidden` and stores both flags. |
| D8 | **Other devices.** Every flag change (hide, unhide, un-hide by a message, peer deleted) publishes the new server frame `ConversationFlags{conversation_id, hidden, peer_deleted}` (field 53) to the user's connections. Durable read-back: flag changes count as changes for `changed_since` (their time is `conversation_view.changed_at` or `deleted_user.deleted_at`); `updated_at` keeps meaning content changes only. |
| D9 | **A new message un-hides.** Appending a message (text, rich or answer; not edits, deletes or system entries) to a direct conversation un-hides it for every member who hid it, and publishes `ConversationFlags` to them. In practice the peer's message brings it back; the user's own message (sent from any device) does too. A deleted peer cannot send, so a conversation hidden after the peer was deleted stays hidden. |
| D10 | **No automatic hiding of conversations with deleted peers.** The server exposes `peer_deleted` and leaves the choice to clients (grey out, hide, or let the user hide it): automatic hiding would remove evidence and history without the user asking, and would need its own un-hide rule. |

## Compatibility

- Old SDKs ignore frame 53 (an unreadable frame is logged and skipped) and list with
  `include_hidden` false, so they simply do not see hidden conversations.
- An old worker poisons `user.deleted` (no handler); an old message-svc answers
  `UNIMPLEMENTED`, which the new worker retries. Deploy the chart as one release (as
  always); the backfilled events are relayed once the new worker and message-svc run.
- Migrations are additive: messaging 0009 creates two new tables (reverse: drop them),
  auth 0010 widens the outbox kind check and inserts events (reverse: delete
  `user.deleted` rows, restore the previous check). Older images ignore the new tables.

## Testing

- auth-core: tombstones only for ids the Directory returned; deleted agent keeps `kind`,
  has the neutral name and empty handle; omitted when not visible; outbox event written on
  delete.
- push-testkit contracts (memory and Postgres): conversation view upsert and un-hide,
  flags with `changed_at`, deleted users.
- push-domain: hide/unhide, listing filter and `peer_deleted`, `changed_since` picks up
  flag changes, a new message un-hides and publishes, deletion publishes to peers.
- message-svc: request mapping and `UserDeleted`; gateway: notification → frame.
- push-sdk: listing stores flags, frame 53 updates the store and emits
  `ConversationChanged`, hide/unhide calls; `Profile.deleted`.
