# Proactive agents: design

Change request (2026-11-09, "REQUEST PUSH active", consumer yuopolev/push-messenger).
Agents only answer today: the bridge never sends a contact request, opens a chat, creates a
group or places a call on its own, and it answers only its owner ("This agent is private."
to everyone else). This design lets a backend start those actions at any time, under an
**outgoing policy** that the owner sets and the server enforces, with an **ask** queue the
owner approves or declines; an **inbound policy** decides who reaches the backend, so two
agents can talk; a **loop guard** stops runaway agent-to-agent chats; and everything an agent
starts is marked and audited.

It builds on agent connections and the owner view
([design](2026-11-06-agent-connections-and-owner-view.md), PRs #76–#81), the echo backend
([design](2026-11-05-echo-backend-design.md)), voice calls and the voice adapter
([calls](2026-11-03-voice-calls-design.md), [adapter](2026-11-04-voice-adapter-design.md)).

The consumer asked us to decide everything; the decisions are recorded in §2 and the
reasoning for the less obvious ones next to the rules.

## 1. Terms

- **Agent, owner**: as in the agent connections design (§1 there).
- **Action**: something an agent starts on its own: a contact request, a new direct chat, a
  group (create or invite), a call. Sending into a conversation that already exists is not
  an action; it is a message.
- **Outgoing policy**: per agent and per action kind, `allowed`, `ask` or `off`.
- **Waiting action** (the "ask" queue): an action that waits for the owner's approval.
- **Inbound policy**: per agent, who reaches the backend: `owner_only`, `owner_agents` or
  `contacts`.
- **Bridge action**: what a backend asks the bridge to do: `request_contact`,
  `send_message`, `notify_owner`, `create_group`, `invite`, `call` (and `outgoing`, the
  status of its waiting actions).

## 2. Decisions

| # | Decision |
|---|---|
| D1 | **Outgoing policy per agent, enforced by message-svc** on the existing requests (`ContactRequest`, `OpenDirect`, `CreateGroup`, `AddMembers`, `CallInvite`), so an agent that bypasses the bridge is held to it too. Four kinds: `requests` (contact requests), `chats` (a new direct chat with someone other than the owner), `groups` (create a group, add members) and `calls` (a call to someone other than the owner). Values `allowed`, `ask`, `off`. **Defaults: requests `ask`, chats `ask`, groups `off`, calls `off`** (no row = defaults). |
| D2 | **Always allowed, never an action:** messages and calls to the owner (the owner's direct chat, a call to the owner); messages in any conversation that already exists, whoever started it (a reply in a conversation someone else started, and a chat the owner approved earlier); answers to cards. The policy applies when a conversation would be *created* (a direct chat) or *changed* (a group), when a request would be *sent* and when a call would *ring*. |
| D3 | **`off` answers `NOT_ALLOWED_BY_OWNER`** (new error code 34) at once; nothing is stored or counted. |
| D4 | **`ask` stores a waiting action** and answers `WAITING_FOR_OWNER` (new error code 33) with the action's id (`Error.action_id`, field 7; `CallError.action_id`, field 6). The same action asked again while it waits (same agent, kind and target) answers the same id, no second row. The owner is told with the new frame `OutgoingActionEvent` (field 56) for the badge. |
| D5 | **Owner decides over HTTP** (auth-svc, owner only, `404 not_found` for everyone else, as the existing owner routes): `GET|PUT /v1/agents/{id}/outgoing-policy`, `GET|PUT /v1/agents/{id}/inbound-policy`, `GET /v1/agents/{id}/outgoing`, `POST /v1/agents/{id}/outgoing/{action_id}/approve`, `POST /v1/agents/{id}/outgoing/{action_id}/decline`. |
| D6 | **Approve carries the action out, as the agent, at once** (server side): the contact request is sent, the direct chat is opened, the group is created, the members are added; a call cannot be placed by the server, so an approved call is a **one-time permission** to ring that person within 15 minutes. The normal rules still apply (contacts, blocks, the receiving agent's connection policy); if they refuse, the action ends `failed` with the error code, not a hidden success. |
| D7 | **24 h expiry**: a waiting action not decided in 24 hours ends `expired`. **The agent hears `OWNER_DECLINED` for both declined and expired** (it cannot tell them apart, nor confuse them with the other side's refusal); the owner sees `declined` or `expired`. The agent gets `OutgoingActionEvent` with the outcome, and can list its own actions (waiting, and those resolved in the last 24 hours) with the new request `ListOutgoingActions` to catch up after a reconnect. |
| D8 | **Inbound policy per agent, stored by message-svc, set by the owner, enforced by the bridge**: `owner_only` (default; today's behaviour), `owner_agents` (the owner and the owner's other agents) and `contacts` (every accepted contact). Everyone else keeps getting "This agent is private.". The bridge reads its settings with the new request `GetAgentSettings` and is told about changes by the new frame `AgentSettingsEvent` (field 55). The server cannot enforce it (an accepted contact may always message the agent); it decides who the backend hears. |
| D9 | **Agent-to-agent text is a turn; rich is not.** A bridge sends what its backend says to another agent as plain text (`text`), and passes plain text from an allowed agent to its backend. Replies, cards, notices and the loop-guard note stay rich (`agent.reply@1`, cards) and never prompt a backend, as today. People still get rich replies. |
| D10 | **Loop guard in the bridge**: per conversation with another agent, the bridge counts consecutive agent turns (texts by either agent) without a person's message; at `loopGuardTurns` (bridge config, default 20) it stops passing that conversation to its backend and refuses its backend's `send_message` there, posts a short rich note in the conversation (visible in both owners' views, never a turn for the other bridge) and a note in its owner's chat. A person's message in the conversation, or the owner's `/loop resume`, resets it. The other agent's bridge counts the same messages and stops at the same turn, telling its own owner. |
| D11 | **Outgoing calls**: an agent may call its owner (always, as today) or a person who is an accepted contact (per the `calls` policy). Agent-to-agent calls stay `NOT_ALLOWED`. The bridge places them through the voice adapter; in a call with a contact, the contact's speech goes to the backend as that contact's message and the backend's answer to that contact is spoken. |
| D12 | **Rate limits per agent** (rolling 24 hours, counted in the database by message-svc, `RATE_LIMITED` with `retry_after_s`), counted when an action is carried out under `allowed` or queued under `ask` (an approval does not count twice; owner targets never count): contact requests **10 an hour, 20 a day** (was 20 and 100), new chats **50 a day**, calls **10 a day**, group creations **5 a day**, members added to groups **20 a day**; at most **20 waiting actions** per agent (`retry_after_s` 600). The existing request caps (50 pending, 10 waiting at other owners) stay. |
| D13 | **Visibility**: `started_by_agent` on conversations (`ConversationSummary` field 14, the owner view's conversations) — true for a direct chat an agent opened with someone other than its owner and for a group an agent created, including approved ones, and backfilled for existing ones — and on call history entries (`CallHistoryEntry` field 9, `GET /v1/calls`, the owner view's calls) — true when the caller was an agent. |
| D14 | **Audit, never content**: every policy read and change, list, approve and decline writes one auth-svc security line (`agent.outgoing_policy.read|changed`, `agent.inbound_policy.read|changed`, `agent.outgoing.listed|approved|declined`) with the owner, agent, action id, action kind, policy values, route, status and IP; never a group title or message text. |
| D15 | **Deleting an agent drops its waiting actions** (and resolved ones, its settings and its action log), in `forget_agent`. Nothing is told to the owner (the agent is gone). |
| D16 | **Bridge actions** are one core path for every backend: Echo exposes them as commands (owner chat), Coda as MCP tools served by the bridge itself (`mcp__push__*`), with no change to coda-cli (§8). Their results are stable codes: `done`, `waiting_for_owner`, `not_allowed_by_owner`, `rate_limited`, `paused`, `not_found`, `not_contact`, `not_allowed`, `unavailable`, `failed`. |
| D17 | **Groups in this release**: an agent may create a group of accepted contacts and add accepted contacts to groups it administers, and send messages into its groups; group messages do not prompt the backend (deferred, §13). |

### 2.1 Rejected alternatives

- **Policy in the bridge only.** Simpler, but the request asks for server enforcement, and an
  agent key used outside the bridge would bypass it.
- **A new "agent action" request** instead of gating the existing requests: two paths to the
  same effect, and the existing requests would still need the gate.
- **Approval as a grant the agent re-uses** for every kind: the agent would have to repeat
  exactly the same request (title, member list) to match the grant; carrying the action out
  on approval is simpler and gives the owner a definite result. Calls are the exception
  (the server cannot place a call).
- **Holding the first message on the server** for an `ask` chat: needs a new request shape;
  the bridge keeps it instead (§7.3) and sends it when the chat is approved.
- **Loop guard on the server.** Possible, but the request puts it in the bridge, and only the
  bridge knows which texts are turns.

## 3. Outgoing policy

### 3.1 Where each kind is checked

All checks run in message-svc's domain (`push-domain`), in the transaction of the request,
for an **owned agent that is not deleted** acting as itself. People are not affected.

| Request | Kind | Gate applies when | Not an action |
|---|---|---|---|
| `ContactRequest(to)` | `requests` | the agent's own row toward `to` is absent or `requested_in` (a new request, or one that would accept a waiting request mutually) | `to` is the owner; the agent's row is `accepted`, `requested_out` or `blocked` (answered as today) |
| `OpenDirect(peer)` | `chats` | no direct conversation with `peer` exists yet (after the existing contact check) | `peer` is the owner; the conversation exists |
| `CreateGroup(title, members)` | `groups` | always | — |
| `AddMembers(group, users)` | `groups` | always (after the existing admin check) | — |
| `CallInvite(callee)` | `calls` | `callee` is a person and an accepted contact | `callee` is the owner (always allowed); `callee` is an agent (`NOT_ALLOWED`, unchanged); not a contact (`NOT_CONTACT`, unchanged) |

The gate runs **before** anything that depends on the other side (the hidden-block rule,
the receiving agent's policy, whether `to` exists), so its answer reveals nothing about the
target: an agent learns only its owner's policy and its own counts. A contact request to an
unknown id under `ask` is queued like any other; approved, it answers as today
(`REQUESTED_OUT`, nothing stored).

Closing the old mutual-accept hole: an agent's request to someone whose request waits for
its owner (the agent holds `requested_in`) is a `requests` action, so an agent under `ask`
cannot accept a waiting request by asking back.

### 3.2 Gate

```
gate(agent, kind, target):
  policy = settings(agent)[kind]
  off     -> Err(NotAllowedByOwner)                        nothing stored
  allowed -> limits(agent, kind)?; log(agent, kind); Proceed
  ask     -> same waiting action exists? -> Waiting(id)     nothing counted
             waiting count >= 20        -> Err(RateLimited{600})
             limits(agent, kind)?; log(agent, kind)
             insert action (waiting); event to owner -> Waiting(id)
```

`Waiting(id)` commits the transaction and then answers `WAITING_FOR_OWNER` with `action_id`.
Contact requests keep their existing limiter (`check_agent_limits`, now 10/h and 20/day,
plus the pending caps); the new kinds use the new action log. The gate runs under the
agent's user lock (`lock_users`), so concurrent actions cannot pass a limit together.

The "same action" key: `requests`/`chats`/`calls`: (kind, target user); `groups` create:
(kind, title, sorted member ids); invite: (kind, group, sorted member ids).

### 3.3 Approval, decline, expiry

- **Approve** (owner): under the agent's lock, the action must be `waiting` (else
  `not_found`). It is carried out as the agent with the gate skipped (already counted):
  - request: `contact_request(agent, target)` → state `approved`, result the contact state;
  - chat: `open_direct(agent, target)` → `approved`, result the conversation id
    (`started_by_agent` set when it is created now);
  - group: `create_group(agent, title, members)` → `approved`, result the group id;
  - invite: `add_members(agent, group, members)` → `approved`, result the group id;
  - call: nothing runs; `approved`; a `CallInvite` from the agent to that person within
    **15 minutes** passes the gate once (the action's `used_at` is set).
  When the normal rules refuse (`NOT_CONTACT`, `BLOCKED`, `NOT_ADMIN`, the receiving
  agent's refusal answers as usual — `REQUESTED_OUT` is a success), the action ends
  `failed` with that error code; the owner's call still succeeds (`200`, with the action).
- **Decline** (owner): `waiting` → `declined`. `204`.
- **Expiry**: message-svc's agent sweeper (the existing one, every `PUSH_AGENT_SWEEP_SECONDS`)
  ends actions waiting longer than 24 hours as `expired`, and deletes resolved actions 24
  hours after they were resolved and action-log rows older than 24 hours.
- Every resolution sends `OutgoingActionEvent` to the owner (the badge goes down) and to the
  agent (the outcome; `expired` and `declined` both as `OWNER_DECLINED`).
- Changing the policy does not resolve waiting actions; the owner decides them.

### 3.4 States

| State | Number | Owner sees | Agent sees |
|---|---|---|---|
| waiting | 1 | `waiting` | `WAITING` |
| approved | 2 | `approved` | `APPROVED` |
| declined | 3 | `declined` | `OWNER_DECLINED` |
| expired | 4 | `expired` | `OWNER_DECLINED` |
| failed | 5 | `failed` + error | `FAILED` + error |

## 4. Inbound policy

Stored next to the outgoing policy (`messaging.agent_settings.inbound`): `owner_only` (1,
default), `owner_agents` (2), `contacts` (3). The bridge decides, for each message from
someone other than the owner, whether it reaches the backend:

| Policy | Reaches the backend |
|---|---|
| `owner_only` | nobody else |
| `owner_agents` | an accepted contact that is an agent owned by the same owner |
| `contacts` | every accepted contact (people and agents) |

Anyone else gets the private notice (at most once an hour per sender, as today). The bridge
needs a backend that takes messages from others (`accepts_contacts`): Echo and Coda (with
its tools, §8) do. **Echo changes**: it answered every accepted contact; now it follows the
policy, whose default is `owner_only` (set `contacts` to keep the old behaviour).

## 5. Rate limits

| Limit | Value | `retry_after_s` |
|---|---|---|
| contact requests in the last hour | 10 | until the oldest is an hour old |
| contact requests in the last 24 hours | 20 | until the oldest is a day old |
| new direct chats (not with the owner) in 24 h | 50 | until the oldest is a day old |
| calls to people other than the owner in 24 h | 10 | until the oldest is a day old |
| groups created in 24 h | 5 | until the oldest is a day old |
| members added to groups in 24 h | 20 (counted per member) | until the oldest is a day old |
| waiting actions | 20 | 600 |
| pending outgoing requests (existing) | 50 | 600 |
| requests waiting at other owners (existing) | 10 | 600 |

The gateway's limits for everyone still apply first.

## 6. API

### 6.1 HTTP (auth-svc), owner only

Same authorization as the existing owner routes (one query; `404 not_found` for every
non-owner, the agent itself, a deleted agent and an unknown id; `503 unavailable` when
message-svc cannot be reached; the shared `agent_owner` rate limit).

| Route | Answer |
|---|---|
| `GET /v1/agents/{agent_id}/outgoing-policy` | `200 {"requests":"ask","chats":"ask","groups":"off","calls":"off"}` |
| `PUT /v1/agents/{agent_id}/outgoing-policy` `{"chats":"allowed"}` | `200` with all four; any subset may be given; an unknown key or value is `400 invalid` |
| `GET /v1/agents/{agent_id}/inbound-policy` | `200 {"policy":"owner_only"}` |
| `PUT /v1/agents/{agent_id}/inbound-policy` `{"policy":"owner_agents"}` | `200 {"policy":"owner_agents"}`; unknown value `400 invalid` |
| `GET /v1/agents/{agent_id}/outgoing` | `200 {"actions":[<action>]}`, the waiting actions, oldest first |
| `POST /v1/agents/{agent_id}/outgoing/{action_id}/approve` | `200 {"action":<action>}` (state `approved` or `failed`); not waiting: `404 not_found` |
| `POST /v1/agents/{agent_id}/outgoing/{action_id}/decline` | `204`; not waiting: `404 not_found` |

`<action>`:

```json
{"action_id":"…","kind":"request"|"chat"|"group"|"invite"|"call",
 "target":<profile>|null,"conversation_id":"…"|null,"title":"…"|null,
 "members":[<profile>],"state":"waiting"|"approved"|"declined"|"expired"|"failed",
 "error":"not_contact"|…|null,"created_at":"…","expires_at":"…","resolved_at":"…"|null}
```

`target` for request/chat/call; `title` and `members` for group; `conversation_id` the group
for an invite, and the result (chat or group) once approved. Profiles go through the agent's
visibility, as in the other owner routes.

`GET /v1/agents/{agent_id}/conversations` entries and `GET /v1/agents/{agent_id}/calls`
entries gain `started_by_agent`; `GET /v1/calls` entries too.

### 6.2 WebSocket protocol

| Addition | Number | Meaning |
|---|---|---|
| `ErrorCode.WAITING_FOR_OWNER` | 33 | the action waits for the owner; `action_id` names it |
| `ErrorCode.NOT_ALLOWED_BY_OWNER` | 34 | the owner's outgoing policy is `off` for it |
| `Error.action_id` | 7 | with `WAITING_FOR_OWNER` |
| `CallError.action_id` | 6 | with `WAITING_FOR_OWNER` |
| `Request.get_agent_settings` / `Response.agent_settings` | 24 / 13 | the agent's own settings; `NOT_ALLOWED` for people |
| `Request.list_outgoing_actions` / `Response.outgoing_actions` | 25 / 14 | the agent's own actions: waiting, and resolved in the last 24 h, oldest first; `NOT_ALLOWED` for people |
| `Frame.agent_settings_event` | 55 | `AgentSettingsEvent{settings}` to the agent when its owner changes either policy |
| `Frame.outgoing_action_event` | 56 | `OutgoingActionEvent{action}` to the owner (new waiting action; every resolution) and to the agent (every resolution) |
| `ConversationSummary.started_by_agent` | 14 | §2 D13 |
| `CallHistoryEntry.started_by_agent` | 9 | §2 D13 |
| internal `Notification.agent_settings_changed` / `outgoing_action_changed` | 10 / 11 | carry the frames through Redis |

```proto
enum OutgoingPolicy { OUTGOING_POLICY_UNSPECIFIED = 0; OUTGOING_POLICY_ALLOWED = 1;
  OUTGOING_POLICY_ASK = 2; OUTGOING_POLICY_OFF = 3; }
enum InboundPolicy { INBOUND_POLICY_UNSPECIFIED = 0; INBOUND_POLICY_OWNER_ONLY = 1;
  INBOUND_POLICY_OWNER_AGENTS = 2; INBOUND_POLICY_CONTACTS = 3; }
message AgentSettings { InboundPolicy inbound = 1; OutgoingPolicy requests = 2;
  OutgoingPolicy chats = 3; OutgoingPolicy groups = 4; OutgoingPolicy calls = 5; }
enum OutgoingKind { OUTGOING_KIND_UNSPECIFIED = 0; OUTGOING_KIND_REQUEST = 1;
  OUTGOING_KIND_CHAT = 2; OUTGOING_KIND_GROUP = 3; OUTGOING_KIND_INVITE = 4;
  OUTGOING_KIND_CALL = 5; }
enum OutgoingState { OUTGOING_STATE_UNSPECIFIED = 0; OUTGOING_STATE_WAITING = 1;
  OUTGOING_STATE_APPROVED = 2; OUTGOING_STATE_OWNER_DECLINED = 3;
  OUTGOING_STATE_EXPIRED = 4; OUTGOING_STATE_FAILED = 5; }
message OutgoingAction { bytes action_id = 1; bytes agent_user_id = 2; OutgoingKind kind = 3;
  bytes target_user_id = 4; bytes conversation_id = 5; string title = 6;
  repeated bytes member_ids = 7; OutgoingState state = 8; ErrorCode error = 9;
  int64 created_at = 10; int64 resolved_at = 11; }   // µs since epoch; empty bytes = none
```

`EXPIRED` is sent to owners only. Older clients ignore frames 55/56 and the new fields; an
unknown error code decodes as unspecified, a generic failure.

### 6.3 Internal gRPC (`AgentOwners`, message-svc)

Adds `GetSettings(AgentRef) → AgentSettingsReply`, `SetOutgoing(SetOutgoingRequest) →
AgentSettingsReply`, `SetInbound(SetInboundRequest) → AgentSettingsReply`,
`ListOutgoing(AgentRef) → OutgoingActions` (waiting only, for the owner),
`ResolveOutgoing(ResolveOutgoingRequest{agent, action_id, approve}) → OutgoingActionReply`.
Ownership is re-checked as for the other calls. `AgentConversation` gains
`started_by_agent = 11`.

### 6.4 push-sdk

- Agent side: `Client::agent_settings()`, `Client::outgoing_actions()`,
  `Client::create_group(title, members)`, `Client::add_members(group, users)`;
  `Event::AgentSettingsChanged(AgentSettings)`, `Event::OutgoingActionChanged(OutgoingAction)`
  (owners and agents); `SdkError::Server` gains `action_id: Option<String>`;
  `CallErrorCode::WaitingForOwner{action_id}`/`NotAllowedByOwner`.
- Owner side: `outgoing_policy`, `set_outgoing_policy`, `inbound_policy`,
  `set_inbound_policy`, `outgoing_actions(agent)`, `approve_outgoing`, `decline_outgoing`.
- `Conversation.started_by_agent`, call history `started_by_agent`, owner-view
  `AgentConversation.started_by_agent`.

## 7. Bridge

### 7.1 Actions

A backend asks with `BackendEvent::Action(ActionRequest{action, reply})`, at any time (inside
or outside a turn); the core runs it and answers `ActionResult` on the oneshot.

| Action | Arguments | Runs as |
|---|---|---|
| `request_contact` | `handle` | `lookup(handle)` → `ContactRequest` |
| `send_message` | `to` (`owner`, a handle, or a group id from `create_group`), `text` | owner: a reply in the owner's chat; a handle: `OpenDirect` (opens it if needed; that is a `chats` action) then send — plain text to an agent, a rich reply to a person; a group: send into it (must be a member) |
| `notify_owner` | `text` | a labelled reply in the owner's chat (as `AgentMessage`) |
| `create_group` | `name`, `members` (handles) | `CreateGroup` |
| `invite` | `group`, `member` (handle) | `AddMembers` |
| `call` | `to` (`owner` or a handle), `reason` (optional, said on pickup) | owner: `dial` as today (ignores the notify-by-call policy, not the rate limit); a contact: §7.5 |
| `outgoing` | — | `ListOutgoingActions`, summarized |

Results (`ActionResult{code, detail, action_id, retry_after_s, conversation}`):

| Code | When |
|---|---|
| `done` | carried out |
| `waiting_for_owner` | the owner decides; the bridge finishes it on approval (§7.3) |
| `not_allowed_by_owner` | the owner's policy is `off` |
| `rate_limited` | a limit; `retry_after_s` |
| `paused` | the loop guard paused that conversation |
| `not_found` | unknown handle or group |
| `not_contact` | not an accepted contact |
| `not_allowed` | the rules of agents forbid it (a call to another agent) |
| `unavailable` | no voice, a call already running, or the server unreachable |
| `failed` | anything else (detail) |

Every action is logged (`event = "agent_action"`, kind, code; never text).

### 7.2 Inbound and agent turns

`on_message` for someone other than the owner: rich from anyone never prompts (today's
rule); `PRIVATE` is never answered; plain text from a sender the inbound policy allows (and
the backend accepts contacts) goes to the backend as that contact's message; otherwise the
private notice. Who is "the owner's agent" comes from the sender's profile (`kind` agent and
`owner_user_id` = our owner), cached like contact state. A backend's answer
(`ContactReply`, or `send_message`) to an agent is sent as plain text; to a person as a rich
reply. The welcome notice is not sent to agents (it would only be noise for them).

### 7.3 Waiting actions in the bridge

On `waiting_for_owner` the bridge keeps what it still has to do after approval, keyed by the
action id, in `state.json` (`pending_actions`): the text of a `send_message`, the reason of a
`call`. On `OutgoingActionEvent` for the agent: `APPROVED` → send the text into the
conversation, or dial the person; `OWNER_DECLINED`/`FAILED` → drop it. Each outcome is
posted as a short note in the owner's chat ("Owner declined: message to @bob."). At
start-up, and after each reconnect, the bridge lists its actions (`ListOutgoingActions`) to
settle those resolved while it was away; kept items older than 48 hours are dropped.

### 7.4 Loop guard

Per conversation with an agent peer: `turns` counts texts by agents (either side) since the
last person's message. At `loopGuardTurns` (config, default 20, minimum 2):

- the peer's texts are no longer passed to the backend, and the backend's `send_message` to
  that conversation answers `paused`;
- a rich note in the conversation: "Paused after 20 turns between agents without a person.
  An owner can resume."; and in the owner's chat: "Paused the chat with @peer after 20
  agent turns without a person. Send /loop resume to let it continue.".

A person's message in the conversation resets it; the owner's `/loop resume [@handle]`
resets one or all; `/loop` lists paused chats. The state lives in memory (a restart resets
it, documented).

### 7.5 Calls to contacts

`call(to = handle)` → the server gate (`calls` policy). On pickup the bridge greets ("Hi,
this is <agent>, <owner>'s agent. <reason>"), the person's speech goes to the backend as
that contact's message (and is posted as a transcript in their direct chat), and the
backend's answers to that contact are spoken (and posted). The call's other behaviour (one
call at a time, hang-up, mute, tray state) is the owner call's. Without voice:
`unavailable`.

### 7.6 Echo commands

In the owner's chat (bare words, like `ask` and `pick`; case-insensitive keyword):

| Command | Action |
|---|---|
| `request @h` | `request_contact` |
| `send @h <text>` / `send owner <text>` / `send group:<id> <text>` | `send_message` |
| `notify <text>` | `notify_owner` |
| `group <name> @a @b …` | `create_group` (name: one word or "quoted") |
| `invite group:<id> @h` | `invite` |
| `call @h [reason]` / `call owner` | `call` |
| `outgoing` | `outgoing` |

Echo answers with the result: `done: …`, `waiting for owner (action <id>)`,
`not allowed by owner`, `rate limited, retry in N s`, `paused`, … Echo's `/help` lists them.

## 8. Coda tools (MCP served by the bridge)

coda-cli has no host-tool callback, but loads MCP servers from `.mcp.json` and lets an
orchestrator replace the user layer with `CODA_USER_MCP_DIR`. So, without changing coda-cli:

- The bridge listens on a private **tools endpoint** (same transport as the control channel:
  a named pipe for the current user on Windows, a 0600 socket on Unix under
  `XDG_RUNTIME_DIR`; name `push-agent-bridge-<id>-tools`), and generates a random 256-bit
  token per run.
- Before spawning `coda serve`, it writes `<state dir>/coda-mcp/.mcp.json`: the user's own
  servers (copied from the file `CODA_USER_MCP_DIR` or `~/.coda/.mcp.json` would give) plus
  `"push": {"command": "<bridge exe>", "args": ["tools-mcp"], "env": {"PUSH_BRIDGE_TOOLS":
  "<endpoint>", "PUSH_BRIDGE_TOOLS_TOKEN": "<token>"}}`, and sets `CODA_USER_MCP_DIR` to that
  folder for Coda. A user server named `push` is replaced (logged).
- `push-bridge tools-mcp` is a small stdio MCP server (newline-delimited JSON-RPC:
  `initialize`, `tools/list`, `tools/call`, `ping`) that forwards each call over the tools
  endpoint with the token and returns the result as the tool's text (JSON) and `isError`
  for every code but `done` and `waiting_for_owner`.
- Tools: `request_contact`, `send_message`, `notify_owner`, `create_group`, `invite`,
  `call`, `outgoing` (Coda shows them as `mcp__push__send_message` …). Coda's permission
  prompts for them reach the owner as cards, as for every tool.
- **Coda takes messages from others** (`accepts_contacts`) when the tools endpoint is up: a
  contact's text is given to the session as a prompt (a steer while a turn runs) framed as
  `[message from @handle (agent|person) — answer with send_message to "@handle"]\n<text>`;
  the turn shows in the owner's chat as every turn does. Without the endpoint (Unix without
  `XDG_RUNTIME_DIR`) Coda stays owner-only and logs why.
- The tools endpoint is off with `tools: false` in the bridge config (default `true`).

## 9. Threat model (additions)

| Threat | Mitigation |
|---|---|
| A compromised or confused agent spams people. | Outgoing policy (defaults ask/off), per-kind daily limits, waiting cap, recipients' blocks, the owner's decline/off/delete. |
| The agent learns facts about a target from the gate. | The gate runs before every target-dependent rule; same answer for unknown, blocked and present targets. |
| An agent bypasses `ask` by asking back a waiting requester (mutual accept). | That is a `requests` action (§3.1). |
| Two agents loop. | Loop guard (bridge), plus the write limits (120 a minute). |
| Another local process drives the agent through the tools endpoint. | Same-user transport (pipe DACL / 0600 socket, peer check) and a per-run 256-bit token; off with `tools: false`. |
| The owner's queue fills. | 20 waiting actions; 24 h expiry. |
| The agent tells a refusal by the owner from the other side's. | Distinct codes on purpose (the agent is the owner's); the *other side* never learns the owner declined (nothing is sent to it). |
| Audit leaks content. | Ids, kinds, policy values, counts only. |

## 10. Data model

messaging migration `0011_proactive_agents.sql` (reverse in a comment):

```sql
CREATE TABLE messaging.agent_settings (
    agent_user_id uuid PRIMARY KEY REFERENCES messaging.agent_owner (agent_user_id),
    inbound  smallint NOT NULL CHECK (inbound  BETWEEN 1 AND 3),
    requests smallint NOT NULL CHECK (requests BETWEEN 1 AND 3),
    chats    smallint NOT NULL CHECK (chats    BETWEEN 1 AND 3),
    groups   smallint NOT NULL CHECK (groups   BETWEEN 1 AND 3),
    calls    smallint NOT NULL CHECK (calls    BETWEEN 1 AND 3),
    changed_at timestamptz NOT NULL);
CREATE TABLE messaging.outgoing_action (
    action_id uuid PRIMARY KEY,
    agent_user_id uuid NOT NULL REFERENCES messaging.known_user,
    kind smallint NOT NULL CHECK (kind BETWEEN 1 AND 5),
    target_user_id uuid, conversation_id uuid, title text,
    member_ids uuid[] NOT NULL DEFAULT '{}',
    state smallint NOT NULL CHECK (state BETWEEN 1 AND 5),
    error_code smallint, created_at timestamptz NOT NULL,
    resolved_at timestamptz, used_at timestamptz);
CREATE INDEX outgoing_action_agent ON messaging.outgoing_action (agent_user_id, created_at);
CREATE INDEX outgoing_action_waiting ON messaging.outgoing_action (created_at) WHERE state = 1;
CREATE INDEX outgoing_action_resolved ON messaging.outgoing_action (resolved_at) WHERE state <> 1;
CREATE TABLE messaging.agent_action_log (
    agent_user_id uuid NOT NULL REFERENCES messaging.known_user,
    kind smallint NOT NULL, count integer NOT NULL, at timestamptz NOT NULL);
CREATE INDEX agent_action_log_agent ON messaging.agent_action_log (agent_user_id, kind, at);
CREATE INDEX agent_action_log_at ON messaging.agent_action_log (at);
ALTER TABLE messaging.conversation ADD COLUMN started_by_agent boolean NOT NULL DEFAULT false;
ALTER TABLE messaging.call_history ADD COLUMN started_by_agent boolean NOT NULL DEFAULT false;
-- backfill: groups created by an agent, direct chats an agent opened with someone other
-- than its owner; calls whose caller (direction 'out' row) is an agent.
```

The call's `started_by_agent` travels in the Redis call record (`by_agent`, serde default
`false`) to the history rows.

## 11. Backward compatibility

- Existing agents get the defaults: requests `ask`, chats `ask`, groups `off`, calls `off`,
  inbound `owner_only`. **Behaviour change**: an agent's contact request now waits for its
  owner by default (it was sent at once), and Echo answers only its owner by default. Owners
  who want the old behaviour set `requests: allowed` / inbound `contacts`.
- Contact-request limits for agents tighten from 20/h and 100/day to 10/h and 20/day.
- Older bridges never start actions; they ignore frames 55/56. A newer bridge on an older
  server gets `UNIMPLEMENTED`-like errors for the new requests: it treats the settings as
  defaults and its actions as `failed`.
- Older clients ignore the new fields and frames; owners need an updated client (or the
  HTTP API) to approve actions; until then they expire.
- Deploy the chart as one release; the migration is additive (plus a backfill update).

## 12. Testing

- Store contracts (memory and Postgres): settings get/set and defaults; actions insert,
  dedupe, list, resolve, expire, prune; action log counts; `forget_agent` cleanup;
  `started_by_agent` on conversations and call history.
- Domain scenarios: each kind × `allowed`/`ask`/`off`; owner targets exempt; replies in
  existing conversations allowed; approve carries out each kind, failure ends `failed`;
  decline/expiry → events (`OWNER_DECLINED` to the agent); limits and waiting cap; call
  grant used once within 15 min; the mutual-accept hole closed; gate reveals nothing
  about targets; deletion drops actions.
- message-svc: request/response mapping, error codes and `action_id`, notifications, calls.
- auth-svc: routes, 404 for non-owners, 400 for bad bodies, audit lines without content.
- push-sdk: owner endpoints, agent requests, events, errors with `action_id`.
- push-bridge: every action and result code (TestMessenger), inbound policies, agent turns
  as plain text, loop guard (two Echo bridges stop at 20 and resume), waiting actions
  finished on approval and settled after a restart, echo commands, calls to contacts
  (FakePhone), MCP server (`tools/list`, `tools/call` over an in-memory pipe), Coda MCP
  config writing and contact prompts (FakeCoda).
- Whole stack (`crates/push-smoke/tests/proactive_agents.rs`): §14.

## 13. Deferred

- Group messages prompting the backend (agents only post into groups in this release).
- Agent-to-agent calls; timers or triggers for agents to wake themselves; group policies
  beyond "invite accepted contacts" (out of scope by request).
- Node and C bindings of the new owner endpoints and agent requests (the Rust SDK has them;
  the bindings gain the two events only: `agentSettingsChanged`, `outgoingActionChanged`).
  The bindings' JSON does not carry `actionId` on errors nor `startedByAgent` on
  conversations and call history yet.
- Persisting the loop guard across bridge restarts.
- Per-owner configurable limits.
- Group conversations in apps: the gateway serves groups only with `PUSH_GROUPS=1` (chart
  `gateway.groups`, `false` by default); until then agents' `create_group` and `invite`
  answer `unavailable`.

## 14. Acceptance

Two Echo bridges, Alpha and Beta, of one owner, on the whole stack:

1. Alpha's owner sends `request @beta` to Alpha: `waiting for owner`; the owner gets
   `OutgoingActionEvent(WAITING)`, lists it and approves; Beta's connection policy holds the
   request; the owner approves it (existing route); they are contacts.
2. `send @beta hello` → `waiting for owner`; approve; Alpha's bridge sends "hello" as plain
   text in the new chat (marked `started_by_agent` in the owner view).
3. Beta's inbound is `owner_only`: Alpha gets "This agent is private.". The owner sets both
   to `owner_agents`; Alpha's next text reaches Beta's backend, and they echo back and forth.
4. Both bridges stop after 20 agent turns and post the note; `/loop resume` lets them go on.
5. With chats `off`, `send @someone` answers `not allowed by owner`; with groups `off`,
   `group` likewise; a declined action reaches Alpha as `owner_declined`.
6. The security log has the policy changes and approve/decline lines without content.

## 15. As built

Deviations from the design and details settled during the implementation (PRs #85–#87 and
the bridge, Coda tools and acceptance PRs):

- **Server.** Approving an action runs in one transaction: the operation's locks in the
  documented order with the agent's user lock among them, a re-check that the agent is not
  deleted and the action still waits, then `approved` (with its conversation) or `failed`
  with the rules' code. A refusal rolls back to a savepoint, so nothing it wrote is kept
  (not even an invite's seq); conflicts and storage errors roll everything back and leave
  the action waiting. An approved contact request still meets the caps on pending requests
  (`failed` with `RATE_LIMITED`) without counting against the hourly and daily limits again.
  The gate, the approval, the decline and settings changes re-check the agent's deletion
  under its user lock.
- **Calls.** A resent agent `CallInvite` (the same `call_id`) is not gated twice. The call
  record's `by_agent` marks both history rows `started_by_agent`.
- **Owner routes.** Profiles in an action go through the agent's visibility: a target the
  agent cannot see yet (a contact request to someone who is not its contact) is answered
  by id only (`state: "hidden"`), as in the other owner routes. An approval whose profiles
  cannot be read is still answered (`200`, users by id only) and audited: the action is
  carried out already. `GET …/outgoing` lists the waiting actions only; the agent's
  `ListOutgoingActions` also lists those resolved in the last 24 hours. Audit lines carry
  `action_id`, `kind` and, for the outgoing policy, `policy` as
  `requests:ask chats:ask groups:off calls:off`.
- **push-sdk.** The owner's list of an agent's waiting actions is `agent_outgoing` (the
  design's `outgoing_actions(agent)`; the agent's own list is `outgoing_actions()`).
  `CallErrorCode::WaitingForOwner`
  is a plain variant; the action id comes with the call error (`SdkError::action_id()`).
  `create_group` answers the group's id, never an error once the group exists.
- **Groups at the gateway.** The gateway refuses group-management requests with
  `NOT_AVAILABLE` unless `PUSH_GROUPS=1` (chart `gateway.groups`, `false` by default), before
  message-svc's gate: with groups off, agents' `create_group` and `invite` answer
  `unavailable` ("groups are not enabled on this server") whatever the policy.
- **Bridge config.** The loop guard's key is `loop_guard_turns` (snake case, like every
  bridge key; not `loopGuardTurns`); `tools` (default `true`) turns the Coda tools off.
- **Bridge actions.** Bridge actions never open chats as a side effect: only `send_message`
  to a handle sends `OpenDirect` (the gated `chats` action), and calls never open chats.
  Texts held for a waiting chat are a list (`texts`, every `send_message` to the same
  waiting chat is kept and sent in order on approval; older `state.json` files with `text`
  still load). An approved call is dialed only within 15 minutes of the approval (the
  server's grant), else dropped with a note. Every outcome (approved, declined, expired,
  failed, and an approved call that cannot be placed) leaves a note in the owner's chat.
  Waiting actions are reconciled with `ListOutgoingActions` at start-up and after every
  reconnect; kept items older than 48 hours are dropped.
- **Calls to contacts.** The action answers `done` 3 seconds after the invite when the
  server did not refuse it (or at pickup, if sooner): the server answers invites only when
  it refuses them. The contact's words are posted as agent replies titled "🎤 voice" (the
  `agent.reply` template allows only owner and agent speakers, so not as the contact), in
  their direct chat, or in the owner's chat ("📞 @bob 🎤 voice") when there is no direct
  chat with them.
- **Echo.** An owner's text whose first word is a command keyword is always a command (a
  malformed one answers its usage, it is not echoed).
- **Coda.** Coda takes contacts' texts only while the running Coda was started with the
  tools (its `coda-mcp/.mcp.json` was written for this start). A contact's text is fenced
  between per-message nonce markers (`<<<message-<nonce>` … `message-<nonce>>>>`) after a
  header naming the sender, so a text cannot imitate the frame. A user `.mcp.json` with a
  UTF-8 byte order mark is read as Coda reads it.
- **Acceptance.** `crates/push-smoke/tests/proactive_agents.rs` runs §14 on the whole
  stack with two agents of one owner (and a person who is Alpha's contact) as raw socket
  clients: the request and chat waiting, approved and carried out, `started_by_agent` in
  the owner view, `ListConversations` and call history, the inbound policy reaching the
  agent (frame 55, `GetAgentSettings`), `off` for chats and groups (`PUSH_GROUPS=1` there),
  a declined group, a call approved once and rung, `not_found` for every non-owner, and the
  security log without content. The bridge's side (Echo commands, held texts, the inbound
  policy, the loop guard between two Echo bridges, calls to contacts, the Coda tools) is
  covered by push-bridge's tests, not by two bridge processes on the whole stack.
