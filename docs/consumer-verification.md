# Building, packaging and verifying push-messenger

Commands for consumers who include push-messenger (for example as a Git submodule) and
run it with their own infrastructure. Nothing here needs GitHub Actions, GHCR, a
particular cloud or a maintainer service: run the commands in your own automation and
publish to your own registry. Chart configuration is in
[deployment-configuration.md](deployment-configuration.md).

## Build

From source (Rust 1.85 or later builds every crate; the tests need Rust 1.88 or later,
for testcontainers, and Docker):

```text
cargo build --release --locked -p push-auth -p push-message-svc -p push-gateway -p push-worker -p push-load
cargo test --workspace --locked
```

Images, one target per binary, from the repository root (BuildKit):

| Target | Purpose |
|---|---|
| `auth`, `message-svc`, `gateway`, `worker` | The services the chart deploys |
| `load` | `push-load`, the explicitly targeted harness below |
| `smoke` | End-to-end smoke test: operator Entra accounts, or DEV ONLY local mode with signing key and dev mailbox |

```text
docker build -f deploy/docker/Dockerfile --target gateway -t <your-registry>/push-messenger/gateway:<your-version> .
```

Tag and push with your own registry and versioning; record the pushed digest of
**each** image (for example from `docker buildx imagetools inspect <ref>` or your
registry) and pin it with `images.<name>.digest`. Build all service images from one
source revision and deploy them together with the chart from that revision.

## Package and render the chart

```text
helm lint deploy/helm/push-messenger --strict -f <your-values.yaml>
helm template <release> deploy/helm/push-messenger --namespace <ns> -f <your-values.yaml> > /dev/null
helm package deploy/helm/push-messenger --version <your-chart-version> --app-version <your-version>
```

With `secrets.existingSecret` the render needs no cluster and emits no Secret; a
chart-managed render contains credentials and must not reach shared logs.

## Verification tools

| Tool | Where | What it proves | Safe on production? |
|---|---|---|---|
| `pwsh deploy/helm/test-chart.ps1` | contributor machine | chart structure and guards | n/a (no cluster) |
| `deploy/minikube/test-network-policy.ps1 -Context <ctx> [-Egress]` | existing cluster with an enforcing CNI | internal API isolation, by live allowed/denied connections; with `-Egress` also the egress rules (`networkPolicy.egress.enabled`) | deploys short-lived probe pods; run deliberately |
| `push-smoke` (operator Entra mode, also available through `helm test`) | authorized deployment | two preexisting identities register/connect, become contacts, message and re-sign-in on the same device | yes, with dedicated test identities; creates devices, contacts and messages |
| `push-smoke` (DEV mode) | local cluster only | the whole flow with the DEV ONLY mailbox and signing key, or the DEV Entra issuer | **no** |
| `push-load` | any target you own or are authorized to test | connections, heartbeats, message/receipt latency, reconnects | yes, with dedicated test accounts and the limits below |

## push-load

`push-load` uses only the public API. It has no default target, discovers nothing,
creates no accounts and never needs the signing key, a mailbox or disabled rate limits.
It never prints tokens, passwords or email addresses.

### Prerequisites

- Written authorization to test the target, and a maintenance or test window agreed
  with its operators.
- **Dedicated test accounts** created through your normal sign-up and email
  verification or preexisting Entra identities, at least `connections / devices-per-account` of them (each account has
  at most 10 active devices). Never use real users' accounts.
- A private directory outside every Git working tree for the accounts file, state file
  and reports (encrypted where available). The state holds refresh tokens.
- For large runs, a generator host sized for the connections: raise the open-file limit
  (`ulimit -n` at least twice the connections) and keep the generator's scheduling lag
  under the reported limit. Split very large tests across several generator hosts, each
  with its own accounts subset and state file; their reports are separate.

The accounts file is JSON Lines, with exactly one credential source per line:

```json
{"email":"load-test@example.com","password":"<password>"}
{"entra_token":"<Entra-access-token>"}
{"entra_token_file":"tokens/operator.jwt"}
```

The examples show alternative entries, not three entries for the same account.
Password accounts keep their existing behavior. Entra entries use `/v1/auth/entra`
and must name **preexisting** identities with handles, enrolled through your normal
application flow. Obtain access tokens for the audience configured in
`auth.signIn.entra.audience`, not Microsoft Graph tokens; see [Entra sign-in](entra-sign-in.md).
Do not supply `email`/`password` together with an Entra source. Empty strings, unknown
fields, ambiguous sources and repeated identities are refused without echoing their
contents.

Token file paths are relative to the **accounts file**, not the process directory.
Files contain only the token (surrounding whitespace/newlines are trimmed). They are
reread whenever sign-in or reauthentication is needed: replace an expired token with
a fresh token for the **same** identity, then rerun `prepare` with the same state file.
The unverified JWT `tid`/`oid` UUID claims supply a stable state key and detect
duplicate identities or identity-changing file replacements; the server alone validates
the token, issuer, audience and permission. Operator Entra tokens never enter state
or reports. State continues to hold device refresh tokens; normal `run` uses those,
not the operator tokens. Keep all input files and state private.

### Prepare (once per campaign; resumable)

```text
push-load prepare --target https://api.example.com --authorized-target api.example.com \
  --accounts /private/accounts.jsonl --state /private/state.json \
  --devices-per-account 10 --sample-pairs 20
```

It signs each account in once (auth-svc allows 60 logins per IP per 15 minutes, so
1,000 accounts take about 4 hours from one IP; the harness waits out every
`Retry-After`), registers its devices and makes the sample pairs contacts with a direct
conversation (at most 50 pairs: invites are limited per user and IP). Interrupt and
rerun it at any time; it continues where it stopped, and it signs devices in again when
a run found their tokens refused. Exit 0 means complete; failures name the accounts
file entry and reason (`invalid_credentials`, `email_not_verified`, `too_many_devices`).

### Run

```text
push-load run --target https://api.example.com --authorized-target api.example.com \
  --state /private/state.json --connections 10000 --ramp-per-second 100 \
  --duration-seconds 1200 --target-build "gateway@sha256:..., chart 1.2.3" \
  --report /private/report-1.json
```

It refreshes every selected device's token once, opens one connection per device at the
ramp rate, keeps each alive with application Pings at the announced heartbeat, and sends
a sample message every `--message-interval-seconds` (default 10) on each pair. Ramp plus
duration is limited to 25 minutes (tokens last 30 minutes; the harness does not
re-authenticate). At the end, or on Ctrl-C (or SIGTERM on Unix), it closes every socket
and writes the report. An interruption during the token refresh lets the refreshes in
flight finish, saves every rotated token and connects nothing. A device whose refresh
failed without an answer is marked for the next `prepare` to sign in again, because
auth-svc may have rotated its token; each refresh is retried only within 25 s, inside
auth-svc's 30 s grace for a repeated token. Killing the process outright can still lose
rotated tokens; the next run then reports those devices and `prepare` repairs them.
Exit 0 only when the run `passed`; 1 `failed`; 130 `interrupted`; 2 a usage or
precondition error. A run that did not pass is never a capacity result.

`--connect-to <ip>` dials another address (for example one load-balancer node) while
keeping the target's host for `Host`, TLS SNI and certificate checks.

### Reconnects during a gateway rollout

To measure reconnect behaviour, start a run with a duration that covers the rollout and,
once the ramp has finished, roll the gateway with your normal process (for example
`kubectl rollout restart deployment/<release>-gateway`). Connections closed with 4005
(shutting down) or dropped reconnect with backoff and jitter; the report counts
disconnects and reconnects and gives reconnect latency. Messages stored during the
rollout must still be delivered: `lost` must stay 0.

### Reading the report

- `status` and `failures`: `passed` requires every connection to have reached Ready and
  all to have been open at once, no connect/reconnect failure, no authentication
  failure, no takeover (4007), no rate limiting, no other Error frame (a refused Send
  or request), every Ping answered within 10 s, no unexpected close code, sample
  messages stored (`SendAck`) and none lost, and an unsaturated generator.
- `latency`: `connect`, `ping_rtt`, `delivery`, `receipt` and `reconnect` as count, p50,
  p90, p99 and max in milliseconds; `percentiles` states the nearest-rank definition.
- `generator.scheduling_lag`: if `saturated`, the generator limited the run and the
  results do not describe the target.
- `workload`, `target`, `target_build`, `started_unix_s` and `elapsed_s` identify what
  was measured. Keep reports with the deployment's own monitoring for the same window.

The harness proves what one run measured. Capacity claims need the consumer's real
deployment, its load balancer and monitoring, repeated runs and their own acceptance
criteria. When the campaign ends, revoke the test devices through your normal account
processes and delete the state file.

### Local functional check

On a local minikube installation (`deploy/minikube/up.ps1`), DEV ONLY accounts come
from `push-dev-accounts` in the `push-smoke` crate, which reads the worker's DEV ONLY
mailbox:

```text
cargo run -p push-smoke --bin push-dev-accounts -- --auth http://127.0.0.1:18080 \
  --host push.local --mailbox http://127.0.0.1:18081 --count 6 --out /private/accounts.jsonl
```

(with `kubectl port-forward` of the ingress controller to 18080 and of the worker to
18081). Then prepare and run against `http://push.local:18080` with
`--connect-to 127.0.0.1`.

## Operator Entra smoke test

Use an accounts JSONL file in the format above containing **exactly two distinct,
preexisting Entra identities** (inline or token files, no passwords). It signs in
through `/v1/auth/entra`, registers and connects two SDK clients, establishes a contact
by invite, sends a message, and signs in again on the first client's existing device.
It neither creates identities nor calls the DEV issuer or password endpoints, and
needs no service signing key or mailbox:

```text
push-smoke --auth https://api.example.com --socket wss://api.example.com/v1/socket --data-dir C:\private\smoke --entra-accounts C:\private\smoke-accounts.jsonl
```

Create the private data directory first. `PUSH_SMOKE_ENTRA_ACCOUNTS` is the equivalent
environment setting. `--entra-token-url` / `PUSH_SMOKE_ENTRA_TOKEN_URL` remains
**DEV ONLY** and is mutually exclusive with operator accounts, including environment
settings. Operator failures suppress HTTP diagnostic bodies to avoid credential echoes.
Each run creates devices and persists encrypted SDK databases; revoke the test devices
and delete that directory after the campaign (accounts have at most ten active devices).

For a consumer `helm test`, provide an existing Secret in the release namespace:
`smoke.entraAccountsSecret: operator-smoke-accounts`. Its `accounts.jsonl` key
(`smoke.entraAccountsKey` overrides the filename) holds the two entries; relative
`entra_token_file` paths refer to other keys in that same Secret. The chart mounts the
whole Secret read-only and passes only the accounts filename, never token values,
to the Pod. Provision/update it through your normal secret-management process; do
not commit tokens in values or put them on command lines. Set `smoke.authUrl` and
`smoke.socketUrl` for your ingress/controller, pin `images.smoke`, and run
`helm test <release> --namespace <namespace> --timeout 6m`. Allow the Pod's ingress
and DNS traffic in any consumer namespace-wide policies. This opt-in works without
dev mode and does not expose the service signing key or worker mailbox.

## Local functional evidence (2026-10-17)

A small real deployment, not a capacity result: branch `feature/plan-11c-verification-load`
at `3dbbbf8`, a fresh minikube profile (Kubernetes v1.31.0, Calico, the local
ingress-nginx compatibility path) installed with `deploy/minikube/up.ps1`, reached
through `kubectl port-forward` of the ingress controller.

1. `push-dev-accounts --count 6` created six DEV ONLY verified accounts through the
   public API and the worker's dev mailbox.
2. `push-load prepare --devices-per-account 2 --sample-pairs 2` prepared 12 of 12
   devices and 2 of 2 pairs (exit 0).
3. `push-load run --connections 12 --ramp-per-second 4 --duration-seconds 180
   --message-interval-seconds 5`, with `kubectl rollout restart deployment/push-gateway`
   60 s into the run, ended `passed` (exit 0):

| Measurement | Result |
|---|---|
| Connections | 12/12 ready, peak 12 open; 12 disconnects (close code 4005 from the rolled gateway), 12 reconnects, 0 failures; fewest open during the rollout: 5 |
| Heartbeats | 60 Pings, 60 Pongs at the 30 s heartbeat; RTT p50 3 ms, p99 85 ms |
| Connect latency | p50 14 ms, p99 60 ms |
| Reconnect latency (losing a connection to Ready again) | p50 564 ms, p90 725 ms, p99 754 ms |
| Sample messages | 68 sent, 68 stored, 68 delivered, 68 receipted, 0 lost |
| Delivery / receipt latency | delivery p50 17 ms, p99 65 ms; receipt p50 115 ms, p99 935 ms |
| Generator | scheduling lag p99 15 ms (limit 250 ms), not saturated |

The report held no token, password or email address. The local port-forward collapses
client addresses and the run is tiny: it proves the harness and the reconnect path,
not production capacity.
