# Portable deployment readiness

Date: 2026-10-16

This refines Plan 11 of the implementation roadmap. It supersedes the assumption
that this public repository must provision Azure or operate a deployment pipeline.
The existing messaging design remains authoritative for service behaviour and the
public `/v1/...` API.

Implementation status: **11A Done (#20)**.
Its [operator upgrade runbook](../../deployment-upgrade.md) is the executable guide
for existing bundled installations. **11B Done (#21)**:
[plan](../plans/2026-10-16-plan-11b-portable-chart-interfaces.md),
[consumer configuration](../../deployment-configuration.md) and
[maintained-controller verification](../../deployment-ingress-verification.md).
**11C Done (#22)**:
[plan](../plans/2026-10-17-plan-11c-verification-load-harness.md) and
[consumer verification](../../consumer-verification.md).

## 1. Goal and ownership

Consumers can include push-messenger as a Git submodule and build their own
scaffolding around it. This repository supplies the services, SDK, Docker build,
one Helm chart, upgrade instructions and reusable verification tools.

Consumers own:

- Infrastructure provisioning, including ARM templates, VMs and clusters.
- Cluster controllers, NetworkPolicy enforcement and persistent storage.
- Secret delivery, including any Azure Key Vault integration.
- Domains, certificates, ingress-controller configuration and cluster metadata.
- CI/CD, version selection, registries, publication and deployment.
- Operational backups, restore procedures and deployment-specific capacity tests.

Using push-messenger must not require GitHub Actions, GHCR, a particular cloud,
or a maintainer-operated release/deployment service. Existing repository CI is
contributor tooling, not a consumer dependency. This work does not remove it.
Consumers can build images with the existing multi-target Dockerfile and package
the chart with Helm in their own automation. Documentation must show the
commands independently of a hosted CI service.

No cloud resources are provisioned in this work. A later consumer-owned Azure
deployment can exercise these interfaces without putting environment-specific
configuration in the submodule.

## 2. Delivery boundaries

Deliver the work through three sequential implementation plans:

| Plan | Delivers |
|---|---|
| 11A: Internal isolation and safe upgrades | Separate gRPC listeners, per-API network isolation, a non-superuser messaging database role, and an explicit data-preserving upgrade procedure |
| 11B: Portable chart interfaces | Existing-Secret support, configurable Ingress/TLS/annotations, image digest references, offline rendering, and a verified maintained-controller routing path |
| 11C: Reusable verification and capacity tools | Consumer-facing build/package/verification commands and an explicitly targeted load harness, without registry publication or deployment workflows |

Each plan retains the existing local smoke workflow and adds focused tests for
its changes. An implementation plan is written and reviewed before that part is
implemented. These are not three independent parallel feature branches.

## 3. Internal API isolation

Kubernetes NetworkPolicy controls connections at the IP/port level, not by gRPC
method. Today `Messaging`, `Accounts` and `Directory` share one port, so their
intended caller boundaries cannot be enforced independently by NetworkPolicy.

Run three listeners in the same message-svc process, sharing the existing domain
service and database pool:

| API | Default port | Only application caller |
|---|---|---|
| Messaging | 50051 | gateway |
| Accounts | 50052 | worker |
| Directory | 50053 | auth-svc |

Each listener registers only its own gRPC service and keeps that service's
existing internal-token authentication. No shared listener remains as a bypass.
The health endpoint remains separate. Startup must bind every configured
listener successfully; readiness must not advertise a partial service. Shutdown
stops all three listeners, allows in-flight work to finish, and closes shared
dependencies after the listeners finish.

Keep `PUSH_GRPC_ADDR` for Messaging. Add `PUSH_ACCOUNTS_GRPC_ADDR` and
`PUSH_DIRECTORY_GRPC_ADDR` for the other listeners. Update the chart and caller
URLs together: the worker's message-svc URL points at Accounts, auth-svc's at
Directory, and the gateway's at Messaging. Public API paths and SDK behaviour do
not change. Document that custom deployments must update internal URLs; old
images and new chart ports are not a supported mixed-version configuration.

Enable application-scoped ingress NetworkPolicies by default, with an explicit
opt-out for consumers who supply equivalent policies. Select this release's pods,
not every pod in the namespace. Restrict each internal API port to its designated
caller in the same namespace and release. Allow Postgres connections from
message-svc, auth-svc and the worker, Redis from its actual application clients,
and public HTTP/WebSocket traffic from explicitly configured ingress-controller
namespace/pod selectors. No wildcard controller selector is silently substituted.

Do not introduce default-deny egress in this change: consumer DNS, email-provider
and external network topology are not known to this chart. Document this limit.
Operators remain responsible for restricting Kubernetes permissions: someone who
can create arbitrary pods with trusted labels or add permissive policies is
outside the protection offered by these policies.

Creating policy YAML is not proof of isolation. Verification must use a CNI that
enforces NetworkPolicy and test allowed and denied connections for every API,
including cross-release and unrelated-pod attempts.

## 4. Database roles and data-preserving upgrades

Message-svc must stop connecting as the PostgreSQL bootstrap superuser. Give
`push_messaging` a dedicated login role with no superuser, role-creation or
database-creation privileges. It owns its database and the objects required for
its existing migrations, but cannot access the auth or worker databases.
Preserve the worker's existing narrow access to auth's outbox and cleanup
functions.

New installations bootstrap the roles and databases before application startup.
Keep bootstrap-admin credentials available only to Postgres and explicit
operator-run maintenance, never to message-svc.

Support existing installations without deleting their volumes. Supply an
explicit operator-run migration procedure with these steps:

1. Confirm the installation and source version, take a restorable database backup,
   and record the current image, chart and secret references without printing
   credentials.
2. Enter a maintenance window and stop application writers, including the worker.
3. Using administrator credentials, create or verify the messaging role, transfer
   ownership of the messaging database and its application objects, and verify
   the intended grants and isolation.
4. Provision the new messaging credentials and connection URL, then deploy the
   coordinated chart and service versions.
5. Run application migrations and verify users, devices, contacts, conversation
   history and outbox state before reopening traffic.

The procedure must identify incompatible pre-existing roles or permissions and
stop explicitly, rather than silently accepting them. Retrying a successfully
completed step must be safe. Application schema migrations remain under the
existing migration mechanisms.

Document rollback checkpoints, including when restoring a backup is required;
do not claim a Helm rollback reverses database ownership or schema changes.
Do not require operators to delete a database, namespace or PVC to upgrade.
Do not promise zero downtime.

## 5. Secrets as a portable interface

Retain generated-and-retained credentials for the existing local workflow.
Add `secrets.existingSecret` to select a consumer-managed Kubernetes Secret in the
release namespace. In this mode the chart:

- References the named Secret and does not create, adopt, mutate or delete it.
- Never generates fallback credentials or renders secret values into workloads.
- Renders offline without `lookup`, including email/webhook configuration.
- Rejects conflicting inline credential values rather than silently ignoring them.

Use this fixed key contract:

| Keys | Purpose |
|---|---|
| `postgres-password` | Bundled Postgres bootstrap administrator only |
| `messaging-db-password`, `database-url` | Dedicated messaging role bootstrap and application connection |
| `auth-db-password`, `auth-database-url` | Auth role bootstrap and application connection |
| `worker-db-password`, `worker-database-url`, `worker-auth-database-url` | Worker role bootstrap and its two database connections |
| `redis-password`, `redis-url` | Redis AUTH and application connections |
| `auth-signing-key`, `auth-link-key` | Existing base64-encoded auth keys |
| `accounts-token`, `directory-token`, `messaging-token` | Existing per-API authentication |
| `resend-api-key` | Required when the Resend sender is selected |
| `resend-webhook-secret` | Required when webhook exposure is enabled |

The messaging `database-url` uses the dedicated role, not the administrator.
Passwords and embedded connection URLs must agree. Document their existing
format constraints, including the bundled database bootstrap's alphanumeric
password requirement.

Every service receives only the keys it actually uses. Public render-time
validation checks configuration structure; it cannot inspect the contents of an
external Secret. Required missing keys therefore stop the consuming workload
with explicit Kubernetes configuration errors. Existing application validation
continues to reject invalid key formats and credentials at startup.
For chart-managed credentials, validate hand-set auth-key formats at render time
instead of deferring malformed values to a running workload.

Make webhook exposure an explicit configuration decision that does not depend
on inspecting a live Secret. When enabled, require the webhook signing-secret
reference and render the worker route. Preserve local DEV ONLY behaviour and its
existing guards; never enable the mailbox outside local mode.

Azure Key Vault, CSI resources and secret synchronization belong to the consumer.
The consumer must materialize the required Secret before application startup and
coordinate credential changes with backing services and workload rollouts.
This feature does not claim automatic or zero-downtime secret rotation.

## 6. Controller-neutral public routing

Keep standard Kubernetes Ingress resources with configurable class, TLS secret
and annotations, including distinct annotations for auth link routes. Preserve
the separate link routes needed to apply token-safe logging configuration.
Do not require arbitrary controller snippets.

All public endpoints remain under `/v1/...`, including the Resend webhook.
Operational probes and the local-only mailbox remain unversioned and are not
public routes.

The chart references an existing TLS Secret for TLS termination at Ingress.
Consumers install their controller and provision certificates. They can disable
chart Ingress resources entirely and supply their own routes, including Gateway
API routes, without chart-provided Gateway API CRDs or cluster-wide objects.
Disabling Ingress does not remove the explicit public URL/token-audience
configuration needed by auth-svc and the gateway.

Ingress-nginx-specific annotations belong to the local compatibility values, not
the controller-neutral production interface. Ingress-nginx maintenance ended in
March 2026; do not recommend it as the production controller. Verify the portable
Ingress path with a maintained controller, initially Traefik, installed only in
the verification environment.

For that verified path, test TLS and WebSocket upgrade, heartbeat/idle timeout
behaviour, token-bearing link requests during upstream errors, and trusted
client-IP forwarding. The consumer configures controller access/error logs so
query tokens do not appear, and configures proxy trust so client-supplied
forwarding headers cannot spoof rate-limit identities. The chart must not claim
that nginx annotations provide these guarantees for another controller.

External load-balancer source-IP preservation, controller logs and proxy trust
remain deployment-specific acceptance checks. Document them as prerequisites,
not guarantees inferred from a local port-forward test.

## 7. Consumer verification and capacity tools

Provide documented commands for source builds, selecting Dockerfile targets,
packaging/rendering the chart, supplying external configuration and running the
verification tools. No particular registry or pipeline is required. Consumers
choose versions and publication policy; this work introduces no automatic version
bumps or GitHub release workflow. Accept immutable image digest references in the
chart so consumers can pin what they built.

Keep local functional verification separate from production capacity testing.
The existing DEV ONLY smoke test needs its mailbox and signing-key access and
must not be represented as a production-safe probe. Reusable production-targeted
tools use explicitly provided test accounts/devices and never need the signing
key, a mailbox endpoint or disabled production rate limits.

The load harness requires an explicitly supplied endpoint and explicit
acknowledgement that the operator owns or is authorized to test the target. No
default public endpoint, automatic discovery or production account generation.
Provide an explicit account/device preparation phase through supported APIs,
with bounded concurrency, resumability and respect for rate limits.
Keep credentials out of source control, command output and reports.

Ramp connections with configured upper limits, a finite duration and connection
timeouts; close sockets and stop workers on completion or interruption. Support
10,000 independent authenticated device connections without unintentionally
taking over the same device. Measure connection successes/failures, active
connections, heartbeat behaviour, sampled message/receipt latency and reconnect
behaviour during a deliberate gateway rollout. Report percentile definitions,
workload, duration, target build, and load-generator saturation alongside results.
Never report a failed or incomplete run as a capacity success.

The portable deliverable is the harness and a small local correctness run.
The full 10,000-connection run, resource sizing and performance acceptance belong
to the later consumer deployment. Do not declare production capacity proven by
rendering templates or merely opening idle sockets.

## 8. Acceptance and failure handling

For 11A, verify listener routing, per-service token checks, enforced network
isolation, fresh-database permissions and a seeded data-preserving upgrade.
The upgrade test must exercise the previous installation layout, not just a
fresh database with the desired role.

For 11B, extend chart checks for offline external-Secret rendering, rejected
conflicting/invalid configuration, TLS on every rendered public route when using HTTPS,
controller-specific annotation placement, image digest references and absence
of consumer infrastructure objects. Run existing local smoke verification and
the maintained-controller path on dedicated test installations.

For 11C, test load-harness targeting guards, bounded execution, cleanup,
authentication failure reporting and result correctness against local fixtures
and a small real deployment. Document preparation, rollout and interpretation.

Use existing Rust tests, formatting, Clippy and chart tests for affected code.
Do not hide unavailable prerequisites or substitute a render test for a network
test. A missing Secret, unenforced policy, failed migration or unavailable target
is a visible failed prerequisite or failed run, never a success-shaped fallback.

## 9. Out of scope

Azure provisioning and Key Vault controllers; consumer cluster metadata;
GitHub-dependent build/release/deployment automation; mandatory artifact hosting;
cluster-wide controller installation; first-class Gateway API templates;
managed-database support; multi-node HA; automatic secret rotation; and claiming
10,000-connection production capacity before a real deployment has measured it.

## 10. References

- Existing system design: `2026-10-11-push-messenger-design.md`.
- Kubernetes NetworkPolicy model and enforcement prerequisites:
  <https://kubernetes.io/docs/concepts/services-networking/network-policies/>.
- Kubernetes ingress-nginx retirement notice:
  <https://kubernetes.io/blog/2025/11/11/ingress-nginx-retirement/>.
