# Agent connection policy and a read-only owner view: design

Change request (2026-11-06, "REQUEST PUSH new feat 1": agent connection policy and a
read-only owner view). Agents connect to people and to other agents under a policy that
their owner chooses. Owners get a read-only view of everything their agents do:
conversations, message history and call history. Everyone talking to someone else's agent
is told that its owner can read the chat.

The consumer's own design (docs/specs/2026-11-06-agent-connections-and-owner-view.md in
their repository) is not available to us. This document designs the server, protocol, SDK
and bridge side from the requirements in the request. It builds on the deleted-account
tombstones, `peer_deleted` and hide/unhide ([design](2026-11-06-tombstones-hide-design.md),
PR #75).

## 1. Terms

- **Agent**: an account with `kind: "agent"` and an owner (`owner_user_id`, a person),
  enrolled through `POST /v1/agents` ([owned agents](2026-10-19-entra-sign-in-design.md#4-api)).
  message-svc knows the pair from `messaging.agent_owner` (the `agent.enrolled` event).
- **Owner**: the person who owns the agent. The owner and the agent are accepted contacts
  from enrollment on.
- **Requester**: whoever sends a contact request (`ContactRequest`): a person or an agent.
- **Waiting request**: a request to an agent that waits for the owner's decision. It is
  stored as the agent's `requested_in` row, exactly as a pending request is stored today.
- **Owner view**: the owner's read-only access to an agent's conversations, history and
  calls.

## 2. Decisions

| # | Decision |
|---|---|
| D1 | **Policy per agent**, stored by message-svc in `messaging.agent_policy(agent_user_id, policy, changed_at)`. Values: `ask` (the default; no row means `ask`, so existing and new agents ask), `auto_own`, `auto_owner_contacts`, `auto_any` and `refuse_all`. Only the owner reads or sets it (§5). |
| D2 | **The server resolves every request to an agent at once** (`Messaging::contact_request`), after the existing block and mutual-request rules, in this order: the owner's own request is always accepted (the owner-agent contact can never be refused); then the policy (§3). The outcome is *accept* (both rows `accepted`, as an accept does today), *refuse* (§3.2) or *hold* (the agent gets a `requested_in` row: a waiting request). |
| D3 | **A refusal is indistinguishable from a decline** (§3.2, §6.1): the requester gets `REQUESTED_OUT`, its row is stored, nothing is told to the agent or the owner, and the row disappears silently after a random delay (default 10 minutes to 6 hours). This is exactly what a decline looks like to the requester. |
| D4 | **The owner decides; the agent cannot.** `ContactAccept` and `ContactDecline` by an agent are refused with the new error code `OWNER_DECIDES` (32). An agent cannot block its owner (`NOT_ALLOWED`, 31), and it cannot block a user whose request waits for the owner (`OWNER_DECIDES`): that would be a decline. `CreateInvite` by an agent is `NOT_ALLOWED` (an invite would bypass the owner's policy), and redeeming an invite that an agent created earlier is `INVITE_NOT_FOUND`. |
| D5 | **Owner actions** over HTTP on auth-svc, under `/v1/agents/{agent_id}/…` (§5): get and set the policy, list waiting requests, approve, decline or block a requester, list contacts, remove a contact. Owner only: everyone else, the agent itself included, gets `404 not_found`, identical to an unknown or deleted agent. |
| D6 | **Owners are told about waiting requests.** Every change of an owned agent's relationships is published to the owner's devices as the new server frame `AgentContactEvent{agent_user_id, user_id, state}` (field 54); `state = REQUESTED_IN` means a request now waits for the owner. push-messenger has no mobile push service, so "push" here is the real-time frame; consumers with their own push map the frame (or poll `GET …/requests`). |
| D7 | **Agents may send requests**, to people and to agents, without their owner's approval; the receiving side's rules apply. Agent-only limits (§4), counted by message-svc in the database, all answered `RATE_LIMITED` (23) with `retry_after_s`: at most 20 requests an hour and 100 a day; at most 50 pending outgoing requests; at most 10 outgoing requests waiting for other owners. The gateway's limit for everyone (30 an hour per user) still applies first. |
| D8 | **Agent to agent: messages and cards only.** A `CallInvite` between two agents that are accepted contacts is refused with `CallError{NOT_ALLOWED}`. Every other refused call stays `NOT_CONTACT`, as today: a person's call to an agent is allowed for its owner only. |
| D9 | **Read-only owner view** over HTTP on auth-svc (§5.3): the agent's conversations, the history of any of them exactly as the agent sees it (the same entries `FetchHistory` returns the agent: rich cards, answers, edits applied, deletions as tombstones) and the agent's call history. Reads only: no read position, delivery cursor, hidden flag or unread count of the agent changes, and there is no write route. History before this feature is included, within normal retention. |
| D10 | **Notice data.** Agent profiles carry `owner_display_name` and `owner_handle` (the owner's current values, read from the owner's own account by auth-svc), next to `owner_user_id`, wherever the agent's profile is answered (`GET /v1/users`, `/v1/users/lookup`, `/v1/users/me`; the owner's own `GET /v1/agents` list needs no notice). `null` for people, tombstones, and an agent whose owner is disabled. Clients must show "Chats with <agent> are visible to its owner, <owner>." (§7). |
| D11 | **Audit, never content.** Every owner-view read, policy read or change and owner action writes one line to auth-svc's security log (`target: "security"`) with the event, the owner, the agent, the conversation or peer when there is one, the count of items answered, the route, the status and the client IP; refused owner calls (404) are logged too. Never a message body, a title or a display name (§6.4). |
| D12 | **Deletion.** Deleting an agent (the existing `user.deleted` relay, `Accounts.UserDeleted`) also removes its relationships: its waiting requests, its own outgoing requests, its contacts and the requesters' and contacts' rows toward it (each peer whose row changed gets a `ContactEvent` with `NONE`); peers' own blocks of the agent are kept. Its policy and request log are deleted. The owner view ends: auth-svc answers `not_found` for a disabled agent and message-svc refuses deleted agents too. Conversations stay, so peers keep the history and see the tombstone and `peer_deleted` (#75). |

## 3. Resolving a request to an agent

### 3.1 Order of rules

`contact_request(from, to)` keeps every existing rule and adds the policy where a new
pending request would be created today:

1. `from == to`: `INVALID`. `to` unknown: `REQUESTED_OUT`, nothing stored (unchanged, §5.5
   of the main design).
2. `from` is an agent: the agent limits of §4 (before anything is stored).
3. `from` blocked `to`: `BLOCKED` (unchanged). Already accepted: `ACCEPTED` (unchanged).
4. `to` blocked `from`: the hidden-block rule (unchanged): `REQUESTED_OUT`, `to` hears
   nothing.
5. `to` had already asked `from` (`to`'s row is `requested_out`): mutual, both `accepted`
   (unchanged; for an agent `to`, the agent's own request was its consent).
6. `to` is an owned agent that is not deleted, and `from` is not its owner: the policy.

| Policy | Accepted at once | Otherwise |
|---|---|---|
| `ask` | — | hold |
| `auto_own` | an agent owned by the same owner | hold |
| `auto_owner_contacts` | the owner's accepted contacts (both rows `accepted`), agents owned by the owner, and agents owned by the owner's accepted contacts | hold |
| `auto_any` | everyone | — |
| `refuse_all` | — | refuse |

The owner's own request to their agent is accepted under every policy (the owner-agent
contact always exists; it is created at enrollment, and an owner who blocked their agent
and unblocked it can ask again). A request to a deleted agent is refused (§3.2).

**Hold** stores `from: requested_out`, `to: requested_in` (today's pending request) and
publishes `ContactEvent(REQUESTED_IN)` to the agent and `AgentContactEvent(REQUESTED_IN)`
to the owner. If the agent already has 200 waiting requests, the request is refused
instead (§3.2), so a flood cannot grow the owner's queue without bound.

The policy applies when a request arrives. Changing the policy later does not resolve
requests already waiting: the owner approves or declines them.

### 3.2 Refusal

A refusal stores the requester's `requested_out` row and a row in
`messaging.refused_request(requester_id, agent_user_id, release_at)` with a uniformly random
`release_at` between `PUSH_AGENT_REFUSAL_MIN_S` (default 600) and
`PUSH_AGENT_REFUSAL_MAX_S` (default 21600) seconds from now. The agent gets no row, no
event; the owner gets none either. A sweeper in message-svc (every 30 s, idempotent across
replicas) deletes due `refused_request` rows together with the requester's row if it is
still `requested_out` toward an agent that has no row back: the requester's pending request
disappears without an event, which is exactly how a decline looks to the requester (spec
§5.5: "a decline is announced only to the user who declined"). A repeated request while
refused keeps the first `release_at`. If the agent itself asks the requester before the
release, the existing mutual rule applies and the refusal row is removed.

### 3.3 Owner actions on requests

- **approve** = `contact_accept(agent, requester)`: both `accepted`, `ContactEvent` to
  both, `AgentContactEvent(ACCEPTED)` to the owner.
- **decline** = `contact_decline(agent, requester)`: as a person's decline.
- **block** = `contact_block(agent, requester)`: as a person's block (works whether or not
  a request is waiting).
- **remove contact** ends any relationship of the agent with the user except the owner's:
  an accepted contact (both rows deleted; `ContactEvent(NONE)` to both: the peer can no
  longer send, so hiding it would only confuse their client), a waiting request (= decline),
  the agent's own pending request (withdrawn: both rows deleted, `ContactEvent(NONE)` to
  both) and a block of the agent's (= unblock). Removing nothing succeeds (idempotent).
  The owner is never removed (`400 owner_contact`).

These run as the agent, under the same pair lock and rules as the agent's own requests, so
they cannot race with requests from the peer.

## 4. Agent limits

message-svc counts each agent's outgoing requests in
`messaging.agent_request_log(agent_user_id, peer_id, sent_at)`; a request is logged when it
passes the limits, whatever its outcome. Before storing anything, a request from an agent
is refused with `RATE_LIMITED` when:

| Limit | Value | `retry_after_s` |
|---|---|---|
| requests in the last hour | 20 | until the oldest of them is an hour old |
| requests in the last 24 hours | 100 | until the oldest of them is a day old |
| pending outgoing requests (`requested_out` rows) | 50 | 600 |
| outgoing requests waiting for another owner (the peer is an agent and holds a `requested_in` row) | 10 | 600 |

Log rows older than 24 hours are deleted by the same sweeper. A request to an id that is not
a user still counts (it would otherwise be a free probe), but is otherwise answered as
before. The limits are checked under the agent's own advisory lock, so concurrent requests
cannot pass them together. The caps are counted for the agent as a whole, before the
target's policy is looked at, so the answer does not depend on the target (§6.1).

## 5. API

### 5.1 HTTP (auth-svc), owner only

All routes take the owner's access token. auth-svc first checks, in one indexed query, that
the agent exists, is an agent, is not disabled and is owned by the caller
(`user_id = $agent AND owner_user_id = $caller AND kind = 'agent' AND disabled_at IS NULL`);
any other case answers `404 {"error":{"code":"not_found",…}}` with no further work, the
same for a stranger, the agent itself, the agent's contacts, a deleted agent and an id that
does not exist. auth-svc then calls message-svc's internal `AgentOwners` API (§5.2). Errors
of the owner's own (an unknown conversation, no waiting request) are `not_found` too.
`503 unavailable` when message-svc cannot be reached.

| Route | Answer |
|---|---|
| `GET /v1/agents/{agent_id}/policy` | `200 {"policy":"ask"}` |
| `PUT /v1/agents/{agent_id}/policy` `{"policy":"auto_owner_contacts"}` | `200 {"policy":"auto_owner_contacts"}`; an unknown value is `400 invalid` |
| `GET /v1/agents/{agent_id}/requests` | `200 {"requests":[{"user":<profile>,"requested_at":"…"}]}`, oldest first |
| `POST /v1/agents/{agent_id}/requests/{user_id}/approve` | `204`; no waiting request: `404 not_found` |
| `POST /v1/agents/{agent_id}/requests/{user_id}/decline` | `204`; no waiting request: `404 not_found` |
| `POST /v1/agents/{agent_id}/requests/{user_id}/block` | `204`; the owner: `400 owner_contact` |
| `GET /v1/agents/{agent_id}/contacts` | `200 {"contacts":[{"user":<profile>,"state":"accepted"\|"requested_out"\|"requested_in"\|"blocked","owner":bool}]}` |
| `DELETE /v1/agents/{agent_id}/contacts/{user_id}` | `204`; the owner: `400 owner_contact` |
| `GET /v1/agents/{agent_id}/conversations` | §5.3 |
| `GET /v1/agents/{agent_id}/conversations/{conversation_id}/messages?before_seq=&limit=` | §5.3 |
| `GET /v1/agents/{agent_id}/calls?before=&limit=` | §5.3 |

`<profile>` is the profile as `GET /v1/users` answers it (§5.4), for every id the agent may
see; a requester is visible to the agent through its `requested_in` row. A user id that
cannot be profiled (for example a deleted human) is answered as
`{"user_id": …, "deleted": true, "state": "deleted", …}`, the tombstone.

### 5.2 Internal gRPC (message-svc), `AgentOwners`

Served on the Directory listener with auth-svc's internal token, like `Directory`,
`Templates` and `Calls`. Every request carries `owner_user_id` and `agent_user_id`;
message-svc checks again that `agent_owner(agent) == owner` and that the agent is not
deleted, else `NOT_FOUND` (defence in depth: auth-svc already checked).

```proto
service AgentOwners {
  rpc GetPolicy(AgentRef) returns (AgentPolicyReply);
  rpc SetPolicy(SetAgentPolicyRequest) returns (AgentPolicyReply);
  rpc ListWaiting(AgentRef) returns (WaitingRequests);
  rpc Resolve(ResolveRequest) returns (AgentOwnersDone);       // APPROVE | DECLINE | BLOCK
  rpc ListContacts(AgentRef) returns (push.v1.ContactList);
  rpc RemoveContact(AgentPeerRequest) returns (AgentOwnersDone);
  rpc Conversations(AgentRef) returns (AgentConversations);
  rpc History(AgentHistoryRequest) returns (push.v1.HistoryPage);
  rpc CallHistory(AgentCallHistoryRequest) returns (CallHistoryPage);
}
enum AgentPolicy { AGENT_POLICY_UNSPECIFIED = 0; AGENT_POLICY_ASK = 1;
  AGENT_POLICY_AUTO_OWN = 2; AGENT_POLICY_AUTO_OWNER_CONTACTS = 3;
  AGENT_POLICY_AUTO_ANY = 4; AGENT_POLICY_REFUSE_ALL = 5; }
```

### 5.3 Owner view

- `GET /v1/agents/{agent_id}/conversations`:
  `{"conversations":[{"conversation_id","kind":"direct"|"group","title"|null,"peer":<profile>|null,"peer_owner":<profile>|null,"member_count","last_activity_at","message_count","hidden","peer_deleted"}]}`,
  newest activity first. Every conversation the agent is or was a member of, hidden ones
  included. `peer` is the other member of a direct conversation; `peer_owner` is the
  peer's owner when the peer is an agent. `message_count` counts the messages the agent
  can see that are not deleted; `last_activity_at` is the conversation's `updated_at`.
- `GET …/conversations/{conversation_id}/messages?before_seq=<seq>&limit=<1..100>`:
  `{"entries":[…],"has_more":bool}`, oldest first, the same page `FetchHistory` gives the
  agent. Each entry: `seq, message_id, client_msg_id, kind ("message"|"system"),
  sender_id, content_type ("text"|"rich"|"answer"|"system"), body (UTF-8 string, null once
  deleted), deleted, client_ts, server_ts, edited_at, edit_seq, answer_seq` (absent fields
  are null). A conversation the agent is not a member of is `not_found`.
- `GET …/calls?before=<cursor>&limit=<1..100>`: the agent's call history, the same shape as
  `GET /v1/calls` (`peer_user_id, direction, started_at, answered_at, ended_at, duration_s,
  outcome, cursor`), as the agent's own `GET /v1/calls` would answer it.

The view calls the same read functions the agent's own requests use (`list_conversations`,
`history`, call history) inside read-only transactions that are rolled back; nothing in it
writes. Owner-view reads are limited to 600 an hour per owner (auth-svc's Redis limiter,
`429 rate_limited`).

### 5.4 Profiles

```json
{"user_id":"…","handle":"coda","display_name":"Coda","kind":"agent",
 "owner_user_id":"…","owner_display_name":"Yuri","owner_handle":"yuri",
 "deleted":false,"state":"active"}
```

`owner_display_name` and `owner_handle` are joined from the owner's account at read time,
so a rename shows at once. The agent and its owner cannot set them. People and tombstones
carry `null` for all three owner fields.

### 5.5 WebSocket protocol

| Addition | Number | Meaning |
|---|---|---|
| `ErrorCode.NOT_ALLOWED` | 31 | the rules of agents forbid it: a call between two agents, an agent's invite, an agent blocking its owner |
| `ErrorCode.OWNER_DECIDES` | 32 | an agent tried to accept, decline or block a request that waits for its owner |
| `Frame.agent_contact_event` | 54 | `AgentContactEvent{agent_user_id, user_id, state}` to the owner: an owned agent's relationship with `user_id` changed; `REQUESTED_IN` = waiting for the owner |
| internal `Notification.agent_contact_changed` | 9 | carries the frame through Redis (`notify:{owner}`) |

`ContactRequest` still answers `Contact{state}` (`REQUESTED_OUT` for held and refused
requests, `ACCEPTED` for auto-accepted ones). `RATE_LIMITED` carries `retry_after_s`.

### 5.6 push-sdk and push-bridge

- push-sdk: `Profile.owner_display_name`, `Profile.owner_handle`; `Client::agent_policy`,
  `set_agent_policy`, `agent_requests`, `approve_agent_request`, `decline_agent_request`,
  `block_for_agent`, `agent_contacts`, `remove_agent_contact`, `agent_conversations`,
  `agent_history`, `agent_calls`; `Event::AgentRequestWaiting{agent, requester}` and
  `Event::AgentContactChanged{agent, user, state}`; `SdkError::OwnerDecides` and
  `SdkError::NotAllowed`.
- push-bridge: never accepts or declines; a request event is logged
  (`contact_request_waiting`) and left to the owner; `OWNER_DECIDES` is a known answer, not
  a failure. Agent peers get messages and cards; the bridge never offers them calls, and
  a bridge reply (rich) never prompts another agent's backend, so two bridges cannot loop.
  The welcome note to a new contact and the contacts' `/help` say "Chats with <agent> are
  visible to its owner, <owner>." The owner's `/help` mentions the owner view.

## 6. Threat model

Assets: who an agent talks to, what is said in its chats (people's messages to the agent),
the owner's policy, the owner's identity, and the queue of waiting requests (owner
attention). Actors: strangers and requesters (people or agents, possibly scripted), the
agent's contacts, other owners and their agents, a compromised agent key, a compromised
owner token, and the operator.

### 6.1 Visibility leaks through policy outcomes

| Threat | Mitigation |
|---|---|
| A requester learns the agent's policy from the answer. | Every outcome answers `ContactRequest` with `REQUESTED_OUT`, except auto-accept (`ACCEPTED`, which any accept reveals anyway). Hold and refusal store the same requester row. |
| Refusal vs decline by timing: a refused request vanishing at once would reveal `refuse_all`. | Refused rows are released after a uniformly random 10 min – 6 h delay, silently, exactly as a decline. Residual: a requester measuring many requests over days can estimate the delay distribution; human declines are also often fast or slow. Operators may widen the range. |
| Refusal vs decline by error codes: a decline sends no event to the requester; neither does a release. A re-request during the hold answers `REQUESTED_OUT` in both cases. | Same codes, same events. |
| Hold vs pending at a person: an agent requester learns that a target holds requests from the waiting-for-owner cap. | The cap is counted before the target is looked at and over all of the agent's requests, so it reveals only aggregate facts about the agent's own earlier requests. Kind `agent` is public on profiles anyway. |
| A full owner queue (200) reveals itself. | A request beyond the queue is refused (indistinguishable, above). |
| Owner view `not_found` vs "agent does not exist": a stranger probes which ids are agents. | One query, one answer (`404 not_found`) for every non-owner case, before any call to message-svc, so time and code are the same. The id itself carries 74 random bits. |
| The agent's contacts reach the owner view. | Ownership is checked against `owner_user_id`; contacts are not owners. Tested. |
| The owner learns things about peers beyond the agent's own sight. | The view answers what the agent itself may see (its conversations, its history range, profiles through the agent's visibility). |

### 6.2 Spam and abuse

- Requests to agents: the gateway limit for everyone (30 an hour per user, 120 per IP),
  the refusal policy, the owner's block, the 200-request queue cap.
- Agents as spammers (a compromised or buggy agent): agent-only limits (§4), counted in
  the database so they hold across gateways and restarts; blocks by recipients; the owner
  can delete the agent.
- Agent loops: no calls between agents; the bridge never answers a rich reply with a
  prompt; the write limits (120 a minute per user) cap any remaining loop.
- Owner-view scraping with a stolen owner token: 600 reads an hour, audit lines per read.

### 6.3 Impersonation

- An agent posing as a person: `kind` is set by the server at enrollment and cannot be
  changed by the agent; clients must badge agents. The notice names the owner.
- An agent posing as another owner's agent: display names are not unique, so the notice
  carries `owner_handle` (unique) next to `owner_display_name`; both come from the owner's
  account at read time (the source of truth), never from the agent or a client.
- A person posing as an agent's owner in a chat: the owner is shown from the profile, not
  from message content.
- The agent cannot accept requests, so a compromised agent key cannot build the agent's
  contact list behind the owner's back (it can still send requests, within the limits;
  the receiving side decides).

### 6.4 Privacy of people chatting with agents

- **Notice.** Every client must show "Chats with <agent> are visible to its owner, <owner>"
  in chats with agents the viewer does not own and on the request screen (§7). The bridge
  says it too.
- **Audit without content.** Audit lines hold ids, counts, routes and times only. The
  owner's reads are visible to the operator as metadata, not as content.
- **Retention.** The owner view reads what message-svc keeps; it adds no copy. History
  follows the normal retention, and a deleted message is a tombstone in the view as well.
  Audit lines follow the operator's log retention (document it in their privacy notice).
- **Scope.** The owner sees only conversations the agent is a member of. A person's other
  chats are never reachable. Group members chatting in a group with an agent are covered
  by the same notice (the agent's profile in the member list).
- **No write path.** The owner cannot post, edit, delete, answer cards, mark read or hide
  as the agent through the view; no such route exists.

### 6.5 Other

- Confused deputy between auth-svc and message-svc: message-svc re-checks ownership.
- Deleted agents: the view and the actions end at once in auth-svc (disabled), and in
  message-svc when `user.deleted` arrives.
- Operator: unchanged trust (the operator can read the database).

## 7. Client guidance (notice)

Clients must show the notice "Chats with <agent display name> are visible to its owner,
<owner_display_name> (@<owner_handle>)." in every direct conversation with an agent the
viewer does not own, in group conversations that include such an agent, and on the
contact-request screen for an agent. When `owner_display_name` is null, show "Chats with
<agent> are visible to its owner." Owners see their own agents' chats without the notice.

## 8. Data model and migrations

messaging migration `0010_agent_connections.sql` (reverse:
`DROP TABLE messaging.agent_request_log, messaging.refused_request, messaging.agent_policy;`):

```sql
CREATE TABLE messaging.agent_policy (
    agent_user_id uuid PRIMARY KEY REFERENCES messaging.agent_owner (agent_user_id),
    policy        smallint NOT NULL CHECK (policy BETWEEN 1 AND 5),
    changed_at    timestamptz NOT NULL
);
CREATE TABLE messaging.refused_request (
    requester_id  uuid NOT NULL REFERENCES messaging.known_user,
    agent_user_id uuid NOT NULL REFERENCES messaging.known_user,
    release_at    timestamptz NOT NULL,
    PRIMARY KEY (requester_id, agent_user_id)
);
CREATE INDEX refused_request_due ON messaging.refused_request (release_at);
CREATE TABLE messaging.agent_request_log (
    agent_user_id uuid NOT NULL REFERENCES messaging.known_user,
    peer_id       uuid NOT NULL,
    sent_at       timestamptz NOT NULL
);
CREATE INDEX agent_request_log_agent ON messaging.agent_request_log (agent_user_id, sent_at);
```

New tables only; no existing row changes, and older images ignore them. auth-svc needs no
migration (owner fields are a join).

## 9. Backward compatibility

- **Existing agents** have no policy row: `ask`. Requests already pending at an agent
  (`requested_in`) become waiting requests the owner sees.
- **Older clients (people)**: nothing changes for them; they ignore the new profile fields
  (JSON) and see `REQUESTED_OUT` as before. They do not show the notice: owners and
  operators should update clients; the bridge says it in the chat.
- **Older owner clients** ignore frame 54 (an unknown frame is logged and skipped) and
  have no owner endpoints; waiting requests simply wait. Owners need an updated client
  (or the HTTP API) to approve them.
- **Older agents/bridges** that tried to accept requests now get `OWNER_DECIDES` (an
  unknown error code decodes as `UNSPECIFIED`, a failed request). The bridge in this repo
  never accepted requests.
- **Calls between agents** were already refused (`NOT_CONTACT`); now `NOT_ALLOWED` when
  they are contacts. Older clients treat an unknown code as a generic failure.
- **Invites created by agents** stop working (`INVITE_NOT_FOUND`).
- **Deploy** the chart as one release. An old message-svc answers `UNIMPLEMENTED` to
  `AgentOwners`, which auth-svc maps to `503 unavailable`. Migrations are additive.

## 10. Testing

- Store contracts (memory and Postgres): policy get/set, refused rows and release, the
  request log and its counts, message counts.
- Domain scenarios for every policy against people and agents (accept, hold, refuse);
  waiting → approve, decline, block; refusal reveals nothing (answer, events, rows after
  release equal a decline's); agent accept/decline/block → `OWNER_DECIDES`; invites;
  limits and caps; owner-view authorization and read state unchanged; agent-to-agent call
  `NOT_ALLOWED`; deletion cleanup.
- message-svc: `AgentOwners` mapping and ownership re-check; sweeper.
- auth-svc: routes, `not_found` for non-owners (stranger, agent, contact, unknown id,
  deleted agent), no write routes (405), audit lines without content, profile fields.
- push-sdk: owner endpoints and the new events; push-bridge: notice text, no loops.
- Whole stack (`crates/push-smoke/tests/agent_connections.rs`): the acceptance scenario
  (§11).

## 11. Acceptance

The whole-stack test uses two bridge-less agents owned by the same person: **Echo** and a
second agent standing in for **Coda** (Coda itself is an external program). Both sign in
and talk over raw sockets, as in `owned_agents.rs`:

1. Echo requests Coda (policy `ask`): Echo sees `REQUESTED_OUT`; the owner gets
   `AgentContactEvent(REQUESTED_IN)` and lists the waiting request; the owner approves;
   Echo and Coda exchange messages; the owner reads that chat in Coda's owner view
   (entries equal what Coda sees) and Coda's read position is unchanged.
2. Peers see `owner_display_name` and `owner_handle` on Coda's profile.
3. With `auto_owner_contacts`, the owner's accepted contact's request is accepted at once.
4. With `refuse_all`, the requester sees `REQUESTED_OUT`, then the row disappears without
   an event (as a decline).
5. A non-owner (a stranger, the agent itself) gets `404 not_found` on every owner-view
   route.
6. Echo's `CallInvite` to Coda is refused with `NOT_ALLOWED`.

## 12. As built (notes from the implementation, PRs B–F)

- **SDK errors.** The agent errors reach Rust callers as `SdkError::Server` with the code
  `owner_decides`, `not_allowed` or `rate_limited` (the SDK names every server code that
  way), not as new variants; call refusals as `CallErrorCode::NotAllowed`.
- **Owner view listing** is the agent's own listing (hidden conversations included): like
  the agent's, it leaves out direct conversations with users the agent blocked; their
  history stays readable by id.
- **Deletion and tombstones.** The agent's contact rows go, so a peer who had only a
  contact row (no conversation) no longer sees the agent at all; peers who shared a
  conversation keep the tombstone and `peer_deleted`.
- **Bridge.** The notice is a reply (`agent.reply@1`) to each contact's first text of a
  session, and the end of the contacts' `/help`. Replies, cards and the "This agent is
  private." refusal of another bridge never prompt the backend or draw a reply.
- **Audit fields.** `event`, `route`, `status`, `code`, `ip`, `user_id` (the caller),
  `agent_id`, `peer_id`, `conversation_id`, `count` and `policy`.
- **Acceptance.** `crates/push-smoke/tests/agent_connections.rs` runs §11 on the whole
  stack with two agents of one owner standing in for Echo and Coda.

## 13. Deferred

- Node and C bindings of the owner endpoints (the Rust SDK has them; the bindings gain the
  new profile fields and events only).
- Mobile push of waiting requests (no push service in push-messenger).
- Per-owner configurable limits; listing refused requests to the owner.
