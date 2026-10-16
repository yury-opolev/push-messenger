# Plan 11B: Portable Chart Interfaces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially; do not create parallel feature branches.

**Goal:** Let consumers render and deploy the existing bundled application with their own Secrets, controller, certificate and immutable images, while preserving local minikube verification.

**Architecture:** Keep one namespaced Helm chart and the existing service/Secret key contracts. Separate chart-managed credential generation from consumer-managed references, make webhook routing declarative, and render standard Ingress resources with explicit TLS and annotation maps. Prove routing behavior on an isolated Traefik installation rather than inferring it from nginx annotations or rendered YAML.

**Tech Stack:** Helm 3, Kubernetes networking.k8s.io/v1 Ingress and NetworkPolicy, existing PowerShell 7/Python 3/PyYAML chart checks, Docker/minikube with Calico, Traefik 3.7.14 (verification chart 42.0.0), existing Rust/MSRV 1.85 services and smoke client.

---

## Authority, execution boundary and prerequisites

Read these before implementation:

- `docs\superpowers\specs\2026-10-16-portable-deployment-readiness-design.md`,
  especially sections 1, 2, 5, 6, 8 and 9.
- `docs\superpowers\plans\2026-10-11-roadmap.md`.
- `docs\deployment-upgrade.md`. Its private Windows named-pipe transport and
  maintenance gate supersede earlier draft upgrade snippets.

Plan 11A is merged in #20 (`9e27443`). Do not reimplement its listeners,
ownership migration or isolation model. This document is a plan, not permission
to start implementation. Review it first, then use the normal branch/PR process.
Start implementation from clean `main` containing this reviewed document:

```powershell
git switch main
git pull --ff-only
git switch -c feature/plan-11b-portable-chart
pwsh .\deploy\helm\test-chart.ps1
```

Expected: existing chart checks pass. Resolve a baseline failure explicitly
before adding red tests. Do not install dependencies unless that command reports
one missing; its existing error names `deploy\helm\requirements-test.txt`.

Consumers own provisioning, ARM templates, cluster metadata, controllers,
certificates, Secret delivery, registries, publication and CI/CD. No cloud
resources, GitHub workflows, registry publication, CSI/ExternalSecret objects,
Gateway API templates/CRDs, cluster-wide application objects or managed-database
support are added. Existing contributor CI remains unchanged. Plan 11C owns
consumer build/package tooling and the bounded load harness; 11B adds only the
examples needed to explain these chart interfaces and focused routing acceptance.

Use dedicated installations for live checks. Do not touch shared profile `push`
or delete/reuse the retained data in `push11a` or `push11a-upgrade`. Use new,
explicitly owned profiles `push11b-local` and `push11b-traefik`. Stop them with
data retained at the end. Do not run `down` to recover from a failed check.
Never publish `.git\task7-evidence`, `.git\task8-evidence`, backups or Secrets.

Every task commit ends with:

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

The snippets below are exact additions/replacement regions, not permission to
overwrite unrelated content. Paths in commands are Windows paths; `/v1/...`,
image repositories, URLs and container paths are not filesystem paths.

## Interface decisions to implement

| Interface | Contract |
|---|---|
| `secrets.existingSecret: ""` | Empty preserves generated-and-retained local credentials. Nonempty selects a Secret in the release namespace and excludes all managed-Secret rendering, lookup and randomness. |
| External credential conflicts | Reject every nonempty inline password, auth key or email credential; report the value's **name**, never its contents. |
| `worker.email.webhook.enabled: false` | Explicitly controls the signing-secret reference and webhook exposure. Set `true` in local values to retain the generated local webhook. Never discover this flag from a Secret. |
| `ingress.className: ""` | Empty omits `spec.ingressClassName`; consumers may use their default class or set an explicit class. Only local compatibility values select `nginx`. |
| `ingress.tls.secretName: push-tls` | Reference an existing namespaced TLS Secret. With chart Ingress enabled and scheme HTTPS, a nonempty name is mandatory and goes on **every** public Ingress. Never generate a certificate. |
| `ingress.annotations` | Common string-valued annotations, applied to every public Ingress. |
| `ingress.gatewayAnnotations`, `authAnnotations`, `authLinkAnnotations`, `webhookAnnotations` | Per-resource string maps; specific keys override common keys. Link overrides do not leak onto ordinary auth routes. No `tpl` evaluation. |
| `ingress.enabled: false` | Omit all chart Ingress resources, retain public URL/audience settings, and allow consumer routing without chart Gateway API resources. |
| `auth.clientIpHeader`, `gateway.clientIpHeader`: `""` | Empty omits the header environment variable (peer-address behavior). Explicitly select `x-forwarded-for` for the verified Traefik topology and `x-real-ip` in local nginx values. |
| `images.<component>.digest: ""` | For auth, messageSvc, gateway, worker and smoke: a nonempty lowercase `sha256:` plus 64 hex digits renders `repository@digest`, taking precedence over the existing default tag. Empty retains `repository:tag`. |
| Bundled images | Keep `postgres.image` and `redis.image` as complete references; they already accept `repository@sha256:...`. Do not add a second competing image interface. |

Retain `ingress.host` and `ingress.scheme` as explicit public identity settings
even when Ingress is disabled; changing them changes email links and token
audience. Do not introduce an unrelated URL configuration model. Default HTTPS
references `push-tls`; a plain default render remains possible, but installation
requires the consumer's certificate and controller/policy selectors.

**Intentional migration:** a previously retained webhook key no longer implies a
route. An installation that used webhooks must set
`worker.email.webhook.enabled: true` before upgrading. Keep its credential even
when disabled, so disabling/re-enabling is not a rotation. There is no automatic
or zero-downtime credential rotation in this plan.

## File map and task dependencies

| File | Responsibility / task |
|---|---|
| `deploy\helm\test-portability.py` (new) | Focused structural render/rejection tests, using existing PyYAML; Task 1 |
| `deploy\helm\test-chart.ps1` | Invoke new checks and update changed baseline assertions; Tasks 1, 3-5 |
| `deploy\helm\push-messenger\values.yaml` | Portable values and accurate comments; Tasks 2-5 |
| `deploy\helm\push-messenger\values-minikube.yaml` | Explicit local nginx compatibility, webhook and trusted-header settings; Tasks 3-4 |
| `deploy\helm\push-messenger\templates\_helpers.tpl` | Secret naming, email/webhook decisions, auth-key validation, annotations and image selection; Tasks 2-5 |
| `deploy\helm\push-messenger\templates\validate.yaml` (new) | Always execute structural validation, including when Ingress is disabled; Task 2 |
| `deploy\helm\push-messenger\templates\secret.yaml` | Entire generation/lookup branch excluded in external mode; Task 2 |
| `deploy\helm\push-messenger\templates\worker.yaml` | Conditional, required email Secret references; Task 3 |
| `deploy\helm\push-messenger\templates\networkpolicy.yaml` | Webhook controller access follows enabled endpoint, including consumer routes; Task 3 |
| `deploy\helm\push-messenger\templates\ingress.yaml` | Standard routes, TLS, merged annotations and optional class; Task 4 |
| `deploy\helm\push-messenger\templates\auth.yaml`, `gateway.yaml` | Configurable trusted-header environment variables; Task 4 |
| `deploy\helm\push-messenger\templates\NOTES.txt` | Distinguish retained chart Secret from externally owned Secret and certificate prerequisites; Task 6 |
| `docs\deployment-configuration.md` (new) | Consumer values, full key contract, offline render and migration instructions; Task 6 |
| `docs\deployment-ingress-verification.md` (new) | Reproducible isolated Traefik/TLS/error-log/IP/heartbeat acceptance; Task 7 |
| `README.md`, `docs\deployment-upgrade.md` | Link contracts and explain changed upgrade/routing prerequisites; Task 6 |
| Roadmap and portable design status | Keep 11A Done; track 11B honestly; Task 8 |

Tasks are sequential: 1 establishes tests; 2 isolates Secret ownership; 3 removes
live email/routing discovery; 4 makes public routing portable; 5 adds image pins;
6 documents the usable contract; 7 proves it live; 8 closes acceptance. Do not
parallelize edits to shared templates or run tests against someone else's cluster.

## Task 1: Establish focused portable-interface regression tests

- [ ] **Step 1: Create `deploy\helm\test-portability.py` with the following code.**

It runs Helm without a kubeconfig, parses complete resources, discards successful
render output, and never prints credential-bearing output on failure. Tests use
synthetic credentials only. The existing default/local suite remains authoritative
for 11A topology, database ownership and local DEV ONLY guards.

```python
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

import yaml

CHART = Path(__file__).resolve().parent / "push-messenger"
parser = argparse.ArgumentParser()
parser.add_argument("--case", choices=["secrets", "email", "ingress", "images"])
args = parser.parse_args()


def render(values, error=None):
    with tempfile.TemporaryDirectory(prefix="push-chart-") as directory:
        path = Path(directory) / "values.json"
        path.write_text(json.dumps(values), encoding="utf-8")
        env = dict(os.environ, KUBECONFIG=str(Path(directory) / "absent"))
        result = subprocess.run(
            ["helm", "template", "portable", str(CHART), "--namespace",
             "consumer", "-f", str(path)],
            env=env, text=True, capture_output=True, timeout=30, check=False,
        )
    if error is not None:
        assert result.returncode != 0, "invalid values unexpectedly rendered"
        assert error in result.stderr, "expected named validation error missing"
        return []
    assert result.returncode == 0, "helm template failed; rendered output withheld"
    return [doc for doc in yaml.safe_load_all(result.stdout) if doc]


def document(docs, kind, suffix):
    return next(doc for doc in docs if doc["kind"] == kind
                and doc["metadata"]["name"] == "portable-" + suffix)


def containers(docs):
    for doc in docs:
        if doc["kind"] in ("Deployment", "StatefulSet"):
            yield doc, doc["spec"]["template"]["spec"]["containers"][0]
        elif doc["kind"] == "Pod":
            yield doc, doc["spec"]["containers"][0]


def external():
    return {"secrets": {"existingSecret": "consumer-credentials"}}


def secrets():
    docs = render(external())
    assert not any(doc["kind"] == "Secret" for doc in docs)
    expected = {
        "postgres": {"postgres-password", "messaging-db-password",
                     "auth-db-password", "worker-db-password"},
        "redis": {"redis-password"},
        "message-svc": {"database-url", "redis-url", "accounts-token",
                        "directory-token", "messaging-token"},
        "auth": {"auth-database-url", "redis-url", "auth-signing-key",
                 "auth-link-key", "directory-token"},
        "gateway": {"redis-url", "messaging-token"},
        "worker": {"worker-database-url", "worker-auth-database-url",
                   "accounts-token"},
    }
    for doc, container in containers(docs):
        component = doc["metadata"]["labels"]["app.kubernetes.io/name"]
        assert "envFrom" not in container
        refs = [entry["valueFrom"]["secretKeyRef"]
                for entry in container.get("env", []) if "valueFrom" in entry]
        assert {ref["key"] for ref in refs} == expected[component], component
        assert all(ref["name"] == "consumer-credentials" for ref in refs)
        assert not any(ref.get("optional", False) for ref in refs)
    assert docs == render(external()), "external mode is not deterministic offline"
    allowed = {"Service", "Deployment", "StatefulSet", "ConfigMap",
               "Ingress", "NetworkPolicy"}
    assert {doc["kind"] for doc in docs} <= allowed
    for section, key in [
        ("postgres", "password"), ("postgres", "messagingPassword"),
        ("postgres", "authPassword"), ("postgres", "workerPassword"),
        ("redis", "password"), ("auth", "signingKey"), ("auth", "linkKey"),
    ]:
        values = external()
        values[section] = {key: "synthetic-conflict"}
        render(values, section + "." + key)
    for key in ("resendApiKey", "webhookSecret"):
        values = external()
        values["worker"] = {"email": {key: "synthetic-conflict"}}
        render(values, "worker.email." + key)
    render({"secrets": {"existingSecret": "bad/name"}}, "secrets.existingSecret")
    for key in ("signingKey", "linkKey"):
        for value in ("not-base64!", "YQ==", "A" * 42 + "=="):
            render({"auth": {key: value}}, "auth." + key)
    for key, value in (("signingKey", "AA==" * 16),
                       ("signingKey", "A" * 44),
                       ("signingKey", "A" * 43 + "B")):
        render({"auth": {key: value}}, "auth." + key)
    # Canonical base64 for 32 and 48 bytes, not actual deployment keys.
    render({"auth": {"signingKey": "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=",
                      "linkKey": "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ"}})


def email():
    for enabled in (False, True):
        values = external()
        values["worker"] = {"email": {"sender": "resend",
                                     "from": "Push <no-reply@example.test>",
                                     "webhook": {"enabled": enabled}}}
        docs = render(values)
        worker = document(docs, "Deployment", "worker")
        env = worker["spec"]["template"]["spec"]["containers"][0]["env"]
        refs = {e["name"]: e["valueFrom"]["secretKeyRef"]
                for e in env if "valueFrom" in e}
        assert refs["PUSH_RESEND_API_KEY"]["key"] == "resend-api-key"
        assert not refs["PUSH_RESEND_API_KEY"].get("optional", False)
        assert ("PUSH_RESEND_WEBHOOK_SECRET" in refs) == enabled
        if enabled:
            assert not refs["PUSH_RESEND_WEBHOOK_SECRET"].get("optional", False)
        routes = [d for d in docs if d["kind"] == "Ingress"
                  and d["metadata"]["name"] == "portable-worker"]
        assert bool(routes) == enabled
    render({"worker": {"email": {"webhook": {"enabled": True}}}},
           "worker.email.webhookSecret")
    render({"worker": {"email": {"sender": "resend"}}},
           "worker.email.resendApiKey")
    render({"worker": {"email": {"webhook": {"enabled": "false"}}}},
           "worker.email.webhook.enabled")


def ingress():
    values = external()
    values["worker"] = {"email": {"webhook": {"enabled": True}}}
    values["ingress"] = {
        "className": "consumer", "host": "api.example.test", "scheme": "https",
        "tls": {"secretName": "consumer-tls"},
        "annotations": {"example.test/common": "yes", "example.test/override": "common"},
        "authLinkAnnotations": {"example.test/links": "only",
                                "example.test/override": "links"},
    }
    docs = render(values)
    routes = [doc for doc in docs if doc["kind"] == "Ingress"]
    assert len(routes) == 4
    for route in routes:
        spec = route["spec"]
        assert spec["ingressClassName"] == "consumer"
        assert spec["tls"] == [{"hosts": ["api.example.test"],
                               "secretName": "consumer-tls"}]
        annotations = route["metadata"]["annotations"]
        link = route["metadata"]["name"].endswith("-auth-links")
        assert annotations["example.test/common"] == "yes"
        assert annotations["example.test/override"] == ("links" if link else "common")
        assert ("example.test/links" in annotations) == link
        assert not any(key.startswith("nginx.ingress.") for key in annotations)
        assert all(path["path"].startswith("/v1/")
                   for rule in spec["rules"] for path in rule["http"]["paths"])
    values["ingress"]["enabled"] = False
    values["ingress"]["tls"]["secretName"] = ""
    docs = render(values)
    assert not any(doc["kind"] == "Ingress" for doc in docs)
    auth = document(docs, "Deployment", "auth")
    env = {e["name"]: e.get("value") for e in
           auth["spec"]["template"]["spec"]["containers"][0]["env"]}
    assert env["PUSH_AUTH_PUBLIC_URL"] == "https://api.example.test"
    assert env["PUSH_AUTH_TOKEN_AUDIENCE"] == "wss://api.example.test/v1/socket"
    assert "PUSH_AUTH_CLIENT_IP_HEADER" not in env
    render({"ingress": {"tls": {"secretName": ""}}}, "ingress.tls.secretName")
    render({"ingress": {"annotations": {"example.test/bad": True}}},
           "ingress.annotations")
    render({"ingress": {"authLinkAnnotations": ["bad"]}},
           "ingress.authLinkAnnotations")


def images():
    digest = "sha256:" + "a" * 64
    names = ["auth", "messageSvc", "gateway", "worker", "smoke"]
    values = external()
    values["images"] = {name: {"repository": "registry.example.test/core/" + name.lower(),
                                "digest": digest} for name in names}
    values["postgres"] = {"image": "registry.example.test/postgres@" + digest}
    values["redis"] = {"image": "registry.example.test/redis@" + digest}
    values["devMode"] = {"enabled": True, "acknowledge": "insecure-local-only"}
    values["ingress"] = {"host": "push.test", "scheme": "http"}
    docs = render(values)
    assert len(list(containers(docs))) == 7
    assert all(c["image"].endswith("@" + digest) for _, c in containers(docs))
    for name in names:
        for bad in ("sha256:abc", "sha512:" + "a" * 64,
                    "sha256:" + "G" * 64, "sha256:" + "a" * 65):
            render({"images": {name: {"digest": bad}}}, "image digest")
    docs = render({})
    assert document(docs, "Deployment", "gateway")["spec"]["template"]["spec"][
        "containers"][0]["image"] == "push-messenger/gateway:dev"


tests = {"secrets": secrets, "email": email, "ingress": ingress, "images": images}
for name, test in tests.items():
    if args.case is None or args.case == name:
        test()
        print("ok   portable " + name)
```

- [ ] **Step 2: Run the new tests individually and confirm the intended red failures.**

```powershell
python .\deploy\helm\test-portability.py --case secrets
python .\deploy\helm\test-portability.py --case email
python .\deploy\helm\test-portability.py --case ingress
python .\deploy\helm\test-portability.py --case images
```

Expected: external mode still creates a Secret; external Resend cannot render;
TLS/annotation maps are absent; digest is ignored. A syntax/import/tool failure
is not an intended red result.

- [ ] **Step 3: Wire the new file into `test-chart.ps1` immediately before its final failure check.**

```powershell
Check 'portable chart interface matrix' {
    & $python.Source (Join-Path $PSScriptRoot 'test-portability.py')
    $LASTEXITCODE -eq 0
}
```

- [ ] **Step 4: Commit only the new test and runner integration.**

```powershell
git add deploy\helm\test-portability.py deploy\helm\test-chart.ps1
git commit -m "test: specify portable chart interfaces" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 2: Separate Secret ownership and validate chart-managed auth keys

- [ ] **Step 1: Add the new default and replace `push.secretName`.**

Add to `values.yaml`:

```yaml
secrets:
  # Existing namespaced Secret; empty keeps chart-managed credentials.
  existingSecret: ""
```

In `_helpers.tpl`:

```gotemplate
{{- define "push.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{- .Values.secrets.existingSecret -}}
{{- else -}}
{{- printf "%s-secrets" .Release.Name -}}
{{- end -}}
{{- end -}}

{{- define "push.checkSecretSelection" -}}
{{- $name := .Values.secrets.existingSecret -}}
{{- if not (kindIs "string" $name) -}}
{{- fail "secrets.existingSecret must be a string" -}}
{{- end -}}
{{- if $name -}}
{{- if or (gt (len $name) 253) (not (regexMatch "^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$" $name)) -}}
{{- fail "secrets.existingSecret must be a Kubernetes Secret name" -}}
{{- end -}}
{{- $inline := dict
  "postgres.password" .Values.postgres.password
  "postgres.messagingPassword" .Values.postgres.messagingPassword
  "postgres.authPassword" .Values.postgres.authPassword
  "postgres.workerPassword" .Values.postgres.workerPassword
  "redis.password" .Values.redis.password
  "auth.signingKey" .Values.auth.signingKey
  "auth.linkKey" .Values.auth.linkKey
  "worker.email.resendApiKey" .Values.worker.email.resendApiKey
  "worker.email.webhookSecret" .Values.worker.email.webhookSecret -}}
{{- range $key, $value := $inline -}}
{{- if $value -}}
{{- fail (printf "%s conflicts with secrets.existingSecret" $key) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "push.checkAuthKey" -}}
{{- $decoded := .value | b64dec -}}
{{- if or (ne ($decoded | b64enc) .value)
    (and .exact (ne (len $decoded) 32))
    (and (not .exact) (lt (len $decoded) 32)) -}}
{{- fail (printf "%s must be canonical standard base64 of %s" .name (ternary "exactly 32 bytes" "at least 32 bytes" .exact)) -}}
{{- end -}}
{{- end -}}
```

Create `templates\validate.yaml` (it emits no Kubernetes object):

```gotemplate
{{- include "push.checkSecretSelection" . -}}
```

- [ ] **Step 2: Exclude the entire current `secret.yaml` body in external mode.**

Prepend these lines **before its first lookup/variable evaluation**, and append
the closing `end` after the entire existing resource:

```gotemplate
{{- include "push.checkSecretSelection" . -}}
{{- if not .Values.secrets.existingSecret -}}
```

```gotemplate
{{- end -}}
```

Inside that branch, immediately after `$signingKey` and `$linkKey` are selected
from inline/retained/generated values, insert:

```gotemplate
{{- include "push.checkAuthKey" (dict "name" "auth.signingKey / auth-signing-key" "value" $signingKey "exact" true) -}}
{{- include "push.checkAuthKey" (dict "name" "auth.linkKey / auth-link-key" "value" $linkKey "exact" false) -}}
```

Keep the existing managed Secret name, keep/adoption metadata, credential
precedence, alphanumeric messaging-password validation and dedicated-role URLs
unchanged. No Secret ownership annotations, data, checksum or Secret-dependent
rollout annotations are added in external mode. Existing workloads already call
`push.secretName`; do not duplicate naming decisions in each workload.

- [ ] **Step 3: Run the selected test.**

```powershell
python .\deploy\helm\test-portability.py --case secrets
```

The ownership/auth-key portions become green; the worker's optional unused email
references remain the expected failure until Task 3. Inspect `_helpers.tpl` and
`secret.yaml` to confirm every `lookup` is inside a managed-only branch after
Task 3; an offline `helm template` alone cannot prove a lookup was never evaluated
(Helm client rendering can return an empty result without contacting Kubernetes).

- [ ] **Step 4: Commit the Secret selection and auth-key changes.**

```powershell
git add deploy\helm\push-messenger\values.yaml deploy\helm\push-messenger\templates\_helpers.tpl deploy\helm\push-messenger\templates\validate.yaml deploy\helm\push-messenger\templates\secret.yaml
git commit -m "feat: reference consumer-managed deployment secrets" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 3: Make email/webhook references deterministic and fail closed

- [ ] **Step 1: Add the explicit webhook flag in both values files.**

Under `worker.email` in default values:

```yaml
    webhook:
      # References resend-webhook-secret; never inferred from a live Secret.
      enabled: false
```

Add to `values-minikube.yaml`:

```yaml
worker:
  email:
    webhook:
      enabled: true
```

Keep generating a DEV ONLY webhook secret when one is absent in managed local
mode, as before. Leave retained `resend-api-key` / `resend-webhook-secret` data in
the managed Secret even when temporarily unused.

- [ ] **Step 2: Replace `push.hasWebhook` and `push.checkEmail` completely.**

```gotemplate
{{- define "push.hasWebhook" -}}
{{- if .Values.worker.email.webhook.enabled -}}true{{- end -}}
{{- end -}}

{{- define "push.checkEmail" -}}
{{- $email := .Values.worker.email -}}
{{- if not (kindIs "bool" $email.webhook.enabled) -}}
{{- fail "worker.email.webhook.enabled must be a boolean" -}}
{{- end -}}
{{- if and (include "push.devMode" .) (ne (int .Values.worker.replicas) 1) -}}
{{- fail "dev mode keeps the DEV ONLY mailbox in memory: use worker.replicas 1" -}}
{{- end -}}
{{- if not (has $email.sender (list "log" "resend")) -}}
{{- fail "worker.email.sender must be log or resend" -}}
{{- end -}}
{{- if eq $email.sender "resend" -}}
{{- if not $email.from -}}
{{- fail "worker.email.sender resend needs worker.email.from" -}}
{{- end -}}
{{- if include "push.devMode" . -}}
{{- fail "dev mode shows every email: use worker.email.sender log with it" -}}
{{- end -}}
{{- end -}}
{{- if not .Values.secrets.existingSecret -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- if and (eq $email.sender "resend") (not (or $email.resendApiKey (get $existing "resend-api-key"))) -}}
{{- fail "worker.email.sender resend needs worker.email.resendApiKey or a retained resend-api-key" -}}
{{- end -}}
{{- if and $email.webhook.enabled (not (include "push.devMode" .)) (not (or $email.webhookSecret (get $existing "resend-webhook-secret"))) -}}
{{- fail "worker.email.webhook.enabled needs worker.email.webhookSecret or a retained resend-webhook-secret" -}}
{{- end -}}
{{- end -}}
{{- end -}}
```

Append to `validate.yaml`:

```gotemplate
{{- include "push.checkEmail" . -}}
```

In `worker.yaml`, remove its duplicate top-level `push.checkEmail` call, then
replace the two currently unconditional optional email key references:

```gotemplate
            {{- if eq .Values.worker.email.sender "resend" }}
            - name: PUSH_RESEND_API_KEY
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: resend-api-key
            {{- end }}
            {{- if include "push.hasWebhook" . }}
            - name: PUSH_RESEND_WEBHOOK_SECRET
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: resend-webhook-secret
            {{- end }}
```

Do not use `optional: true` or `envFrom`. External key existence is a Kubernetes
startup precondition, not something the chart can inspect offline.

- [ ] **Step 3: Align NetworkPolicy with consumer-managed routes.**

In `networkpolicy.yaml`, replace only the condition setting
`$workerControllers`:

```gotemplate
{{- if include "push.hasWebhook" . -}}
{{- $workerControllers = $controllers -}}
{{- end -}}
```

This intentionally admits configured controller peers when the webhook endpoint
is enabled even if the consumer supplies the Ingress/Gateway routes. It does not
add peers or ports and does not expose `/dev/mailbox`. Keep the DEV ONLY smoke
peer separate.

- [ ] **Step 4: Update existing runner cases at their call sites.**

For each `$withWebhookControllers`, `$withoutIngress` and `$webhook` render in
`test-chart.ps1`, add the argument:

```text
--set worker.email.webhook.enabled=true
```

Rename the existing disabled-Ingress policy case to
`network: enabled webhook admits configured consumer routing peers without chart Ingress`
and replace its body:

```powershell
PolicyMatrix $withoutIngress 'push' $controllerPeers $true
```

Keep the existing no-webhook defaults and DEV ONLY cases. Add these focused
checks to `email()` in `test-portability.py`:

```python
values = external()
values["worker"] = {"email": {"webhook": {"enabled": True}}}
values["ingress"] = {"enabled": False}
values["networkPolicy"] = {"ingressControllerPeers": [{
    "namespaceSelector": {"kubernetes.io/metadata.name": "edge"},
    "podSelector": {"app.kubernetes.io/name": "traefik"},
}]}
docs = render(values)
policy = document(docs, "NetworkPolicy", "worker")
assert policy["spec"]["ingress"][0]["ports"] == [{"protocol": "TCP", "port": 8080}]
assert not any(doc["kind"] == "Ingress" for doc in docs)
```

- [ ] **Step 5: Verify secrets/email tests and then the existing chart suite.**

```powershell
python .\deploy\helm\test-portability.py --case secrets
python .\deploy\helm\test-portability.py --case email
pwsh .\deploy\helm\test-chart.ps1
```

Expected: secrets/email green; the new ingress/images cases still fail until
their tasks. Existing isolation/default/local tests stay green. Review all
`lookup` sites: only managed credentials may reach them. No email/helper/NOTES
path may inspect an external Secret.

- [ ] **Step 6: Commit email behavior and its dependent policy/test changes.**

```powershell
git add deploy\helm
git commit -m "feat: make webhook exposure explicit and offline" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 4: Render controller-neutral Ingress, TLS and trusted-header configuration

- [ ] **Step 1: Replace the ingress defaults and add trusted-header values.**

Replace the current `ingress` block in `values.yaml`:

```yaml
ingress:
  enabled: true
  className: ""
  host: push.example.com
  scheme: https
  tls:
    # Consumer-provisioned Secret; used by every public route for HTTPS.
    secretName: push-tls
  annotations: {}
  gatewayAnnotations: {}
  authAnnotations: {}
  authLinkAnnotations: {}
  webhookAnnotations: {}
```

Add `clientIpHeader: ""` to each existing `auth` and `gateway` block. Remove
`ingress.timeoutSeconds`; timeouts are controller-specific configuration.

Extend the local values' existing `ingress` block and add local trusted headers:

```yaml
ingress:
  className: nginx
  host: push.local
  scheme: http
  tls:
    secretName: ""
  gatewayAnnotations:
    nginx.ingress.kubernetes.io/proxy-read-timeout: "120"
    nginx.ingress.kubernetes.io/proxy-send-timeout: "120"
    nginx.ingress.kubernetes.io/proxy-http-version: "1.1"
  authAnnotations:
    nginx.ingress.kubernetes.io/proxy-body-size: "16k"
  authLinkAnnotations:
    nginx.ingress.kubernetes.io/enable-access-log: "false"
    nginx.ingress.kubernetes.io/proxy-body-size: "16k"
  webhookAnnotations:
    nginx.ingress.kubernetes.io/proxy-body-size: "256k"

auth:
  clientIpHeader: x-real-ip
gateway:
  clientIpHeader: x-real-ip
```

Do not duplicate existing `ingress`, `worker`, `auth` or `gateway` YAML keys.
Merge additions into their blocks. Nginx compatibility remains **local only**;
disabling its access log is not a claim about all error-log paths.

- [ ] **Step 2: Add routing validation and annotation composition helpers.**

Append to `_helpers.tpl`:

```gotemplate
{{- define "push.checkIngress" -}}
{{- $ingress := .Values.ingress -}}
{{- if not (kindIs "bool" $ingress.enabled) -}}
{{- fail "ingress.enabled must be a boolean" -}}
{{- end -}}
{{- if not (and (kindIs "string" $ingress.host) $ingress.host) -}}
{{- fail "ingress.host must be a nonempty public hostname" -}}
{{- end -}}
{{- if not (kindIs "string" $ingress.className) -}}
{{- fail "ingress.className must be a string" -}}
{{- end -}}
{{- if not (kindIs "string" $ingress.tls.secretName) -}}
{{- fail "ingress.tls.secretName must be a string" -}}
{{- end -}}
{{- $_ := include "push.publicUrl" . -}}
{{- if and $ingress.enabled (eq $ingress.scheme "https") (not $ingress.tls.secretName) -}}
{{- fail "ingress.tls.secretName is required for HTTPS chart Ingress" -}}
{{- end -}}
{{- range $name := list "annotations" "gatewayAnnotations" "authAnnotations" "authLinkAnnotations" "webhookAnnotations" -}}
{{- $map := index $ingress $name -}}
{{- if not (kindIs "map" $map) -}}
{{- fail (printf "ingress.%s must be a string-valued annotation map" $name) -}}
{{- end -}}
{{- range $key, $value := $map -}}
{{- if not (kindIs "string" $value) -}}
{{- fail (printf "ingress.%s must be a string-valued annotation map" $name) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- range $name := list "auth" "gateway" -}}
{{- $header := index $.Values $name "clientIpHeader" -}}
{{- if not (kindIs "string" $header) -}}
{{- fail (printf "%s.clientIpHeader must be a string" $name) -}}
{{- end -}}
{{- if and $header (not (regexMatch "^[!#$%&'*+.^_`|~0-9A-Za-z-]+$" $header)) -}}
{{- fail (printf "%s.clientIpHeader must be an HTTP header name" $name) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "push.ingressAnnotations" -}}
{{- $annotations := mergeOverwrite (deepCopy .root.Values.ingress.annotations) .specific -}}
{{- with $annotations }}
annotations:
  {{- toYaml . | nindent 2 }}
{{- end -}}
{{- end -}}
```

Append to `validate.yaml`:

```gotemplate
{{- include "push.checkIngress" . -}}
```

Validate public identity even with `ingress.enabled=false`; the disabled-Ingress
case must not accidentally disable DEV ONLY/public-URL guards.

- [ ] **Step 3: Replace `ingress.yaml` with one explicit standard-Ingress route table.**

This keeps the existing object names, exact link routes, path types and named
service ports without duplicating TLS/metadata four times:

```gotemplate
{{- if .Values.ingress.enabled -}}
{{- $routes := list
  (dict "name" "gateway" "component" "gateway" "paths" (list "/v1/socket") "type" "Exact" "annotations" .Values.ingress.gatewayAnnotations)
  (dict "name" "auth" "component" "auth" "paths" (list "/v1/auth" "/v1/devices" "/v1/users") "type" "Prefix" "annotations" .Values.ingress.authAnnotations)
  (dict "name" "auth-links" "component" "auth" "paths" (list "/v1/auth/verify" "/v1/auth/password/reset") "type" "Exact" "annotations" .Values.ingress.authLinkAnnotations)
-}}
{{- if include "push.hasWebhook" . -}}
{{- $routes = append $routes (dict "name" "worker" "component" "worker" "paths" (list "/v1/webhooks/resend") "type" "Exact" "annotations" .Values.ingress.webhookAnnotations) -}}
{{- end -}}
{{- range $route := $routes }}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: {{ include "push.name" (dict "root" $ "component" $route.name) }}
  labels:
    {{- include "push.labels" (dict "root" $ "component" $route.component) | nindent 4 }}
  {{- include "push.ingressAnnotations" (dict "root" $ "specific" $route.annotations) | nindent 2 }}
spec:
  {{- with $.Values.ingress.className }}
  ingressClassName: {{ . | quote }}
  {{- end }}
  {{- if eq $.Values.ingress.scheme "https" }}
  tls:
    - hosts:
        - {{ $.Values.ingress.host | quote }}
      secretName: {{ $.Values.ingress.tls.secretName | quote }}
  {{- end }}
  rules:
    - host: {{ $.Values.ingress.host | quote }}
      http:
        paths:
          {{- range $route.paths }}
          - path: {{ . }}
            pathType: {{ $route.type }}
            backend:
              service:
                name: {{ include "push.name" (dict "root" $ "component" $route.component) }}
                port:
                  name: http
          {{- end }}
{{- end }}
{{- end }}
```

- [ ] **Step 4: Replace hard-coded IP-header entries, not application IP parsing.**

In `auth.yaml`:

```gotemplate
            {{- with .Values.auth.clientIpHeader }}
            - name: PUSH_AUTH_CLIENT_IP_HEADER
              value: {{ . | quote }}
            {{- end }}
```

In `gateway.yaml`:

```gotemplate
            {{- with .Values.gateway.clientIpHeader }}
            - name: PUSH_CLIENT_IP_HEADER
              value: {{ . | quote }}
            {{- end }}
```

Remove the adjacent nginx-specific claims. Existing services already implement
last-header-line/last-comma-entry parsing; do not add another proxy-trust parser.
NetworkPolicy and controller forwarding/trust configuration must prevent
untrusted direct callers from making this configurable header authoritative.

- [ ] **Step 5: Update old expectations and extend focused routing tests.**

In the existing default gateway check, replace the required `x-real-ip`
assertion with:

```powershell
$gateway -notmatch 'PUSH_CLIENT_IP_HEADER'
```

The existing default auth routing assertion should still check the public
paths, Exact link routes and no `/dev` path, but replace its expectation of an
nginx logging annotation with:

```powershell
$default -notmatch 'nginx\.ingress\.kubernetes\.io/'
```

Keep the current local timeout assertion. Add a local-only annotation check:

```powershell
Check 'local compatibility: link-only nginx log override and explicit trusted headers' {
    (Doc $dev 'Ingress' 'push-auth-links') -match 'enable-access-log: "false"' -and
    (Doc $dev 'Ingress' 'push-auth') -notmatch 'enable-access-log' -and
    (Doc $dev 'Deployment' 'push-auth') -match 'name: PUSH_AUTH_CLIENT_IP_HEADER\s+value: "x-real-ip"' -and
    (Doc $dev 'Deployment' 'push-gateway') -match 'name: PUSH_CLIENT_IP_HEADER\s+value: "x-real-ip"'
}
```

Append to `ingress()` in the new test:

```python
docs = render({})
assert all("ingressClassName" not in d["spec"]
           for d in docs if d["kind"] == "Ingress")
for section, variable in (("auth", "PUSH_AUTH_CLIENT_IP_HEADER"),
                          ("gateway", "PUSH_CLIENT_IP_HEADER")):
    docs = render({section: {"clientIpHeader": "x-forwarded-for"}})
    env = document(docs, "Deployment", section)["spec"]["template"]["spec"][
        "containers"][0]["env"]
    assert next(e["value"] for e in env if e["name"] == variable) == "x-forwarded-for"
    render({section: {"clientIpHeader": "bad header"}}, section + ".clientIpHeader")
```

Add a path-table assertion to the same test, using the earlier four-route
render before disabling Ingress:

```python
expected = {
    "portable-gateway": [("/v1/socket", "Exact", "portable-gateway")],
    "portable-auth": [(p, "Prefix", "portable-auth")
                      for p in ("/v1/auth", "/v1/devices", "/v1/users")],
    "portable-auth-links": [(p, "Exact", "portable-auth")
                            for p in ("/v1/auth/verify", "/v1/auth/password/reset")],
    "portable-worker": [("/v1/webhooks/resend", "Exact", "portable-worker")],
}
for route in routes:
    actual = [(p["path"], p["pathType"], p["backend"]["service"]["name"])
              for rule in route["spec"]["rules"] for p in rule["http"]["paths"]]
    assert actual == expected[route["metadata"]["name"]]
```

- [ ] **Step 6: Run routing tests and commit.**

```powershell
python .\deploy\helm\test-portability.py --case ingress
pwsh .\deploy\helm\test-chart.ps1
git add deploy\helm
git commit -m "feat: make ingress TLS annotations and proxy headers portable" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: routing and all pre-existing checks pass; only the new digest case is
still red. A renderer cannot establish controller logging or trusted-client-IP
behavior; those remain Task 7.

## Task 5: Accept immutable image references without a registry dependency

- [ ] **Step 1: Add `digest: ""` beside the existing tag for all five application images.**

The resulting shape for each entry is:

```yaml
  auth:
    repository: push-messenger/auth
    tag: dev
    digest: ""
```

Use the existing repository for messageSvc, gateway, worker and smoke; keep
`pullPolicy` and local tags unchanged. Document digest precedence in the
`images` block, not in five duplicated comments.

- [ ] **Step 2: Replace `push.image` with the following implementation.**

```gotemplate
{{- define "push.image" -}}
{{- $repository := required "image repository is required" .repository -}}
{{- if contains "@" $repository -}}
{{- fail "image repository must not include a digest; use image digest" -}}
{{- end -}}
{{- $digest := .digest | default "" -}}
{{- if $digest -}}
{{- if not (regexMatch "^sha256:[a-f0-9]{64}$" $digest) -}}
{{- fail "image digest must be sha256 followed by 64 lowercase hexadecimal digits" -}}
{{- end -}}
{{- printf "%s@%s" $repository $digest -}}
{{- else -}}
{{- printf "%s:%s" $repository (required "image tag is required without an image digest" .tag) -}}
{{- end -}}
{{- end -}}
```

The four service templates and smoke hook already call this helper. The smoke
digest must also be validated when its hook is disabled: append this to
`validate.yaml`:

```gotemplate
{{- range $name := list "auth" "messageSvc" "gateway" "worker" "smoke" -}}
{{- $_ := include "push.image" (index $.Values.images $name) -}}
{{- end -}}
```

Do not claim that a digest exists, is signed or supports an architecture simply
because its syntax is valid. Consumers supply registry authorization through
their own workload/service-account mechanisms and verify their artifacts.
No build, login, push or registry lookup runs from Helm.

- [ ] **Step 3: Run the digest tests and the entire chart suite.**

```powershell
python .\deploy\helm\test-portability.py --case images
pwsh .\deploy\helm\test-chart.ps1
```

Expected: all tests pass, including the previously unused smoke image's invalid
digest rejection and preserved `:dev` behavior.

- [ ] **Step 4: Commit.**

```powershell
git add deploy\helm
git commit -m "feat: support immutable application image digests" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 6: Document the complete consumer contract and upgrade transition

- [ ] **Step 1: Create `docs\deployment-configuration.md` with the following contract.**

Begin with ownership/out-of-scope statements from this plan, followed by this
complete Secret table. Names are fixed, not customizable per key. Kubernetes
Secret `data` values have an outer Kubernetes base64 encoding; decoded auth key
values themselves contain standard base64. Do not double-encode `stringData`.

| Key | Required by / condition | Decoded value contract |
|---|---|---|
| `postgres-password` | bundled Postgres | bootstrap administrator `push` password |
| `messaging-db-password` | Postgres empty-volume initialization | `push_messaging` role password |
| `database-url` | message-svc | `postgres://push_messaging:<password>@<release>-postgres:5432/push_messaging` |
| `auth-db-password` | Postgres empty-volume initialization | `push_auth` role password |
| `auth-database-url` | auth-svc | `postgres://push_auth:<password>@<release>-postgres:5432/push_auth` |
| `worker-db-password` | Postgres empty-volume initialization | `push_worker` role password |
| `worker-database-url` | worker | `postgres://push_worker:<password>@<release>-postgres:5432/push_worker` |
| `worker-auth-database-url` | worker | `postgres://push_worker:<same-password>@<release>-postgres:5432/push_auth` |
| `redis-password` | Redis | Redis AUTH password |
| `redis-url` | message-svc, auth-svc, gateway | `redis://:<same-password>@<release>-redis:6379` |
| `auth-signing-key` | auth-svc; DEV ONLY smoke | standard base64 of exactly 32 bytes (Ed25519 seed) |
| `auth-link-key` | auth-svc | standard base64 of at least 32 bytes |
| `accounts-token` | worker, message-svc | 32-256 visible ASCII characters |
| `directory-token` | auth-svc, message-svc | 32-256 visible ASCII characters, distinct from both other API tokens |
| `messaging-token` | gateway, message-svc | 32-256 visible ASCII characters, distinct from both other API tokens |
| `resend-api-key` | worker when sender is `resend` | consumer's Resend API credential |
| `resend-webhook-secret` | worker when `webhook.enabled=true` | valid Svix `whsec_...` signing secret |

Document that the bundled setup requires ASCII alphanumeric passwords; role
passwords, Redis AUTH and embedded URLs must agree. URL hosts depend on the
actual Helm release and namespace. These settings do not add external-database
support or validate credentials against backing services.

Include this non-secret consumer values example (deliberately uses tag mode;
show the digest override immediately below it):

```yaml
secrets:
  existingSecret: consumer-credentials
ingress:
  enabled: true
  className: traefik
  host: messenger.example.com
  scheme: https
  tls:
    secretName: messenger-tls
  annotations:
    traefik.ingress.kubernetes.io/router.entrypoints: websecure
auth:
  clientIpHeader: x-forwarded-for
gateway:
  clientIpHeader: x-forwarded-for
worker:
  email:
    sender: resend
    from: "Messenger <no-reply@example.com>"
    webhook:
      enabled: true
networkPolicy:
  ingressControllerPeers:
    - namespaceSelector:
        kubernetes.io/metadata.name: edge
      podSelector:
        app.kubernetes.io/name: traefik
images:
  auth:
    repository: registry.example.com/messenger/auth
    tag: consumer-release
  messageSvc:
    repository: registry.example.com/messenger/message-svc
    tag: consumer-release
  gateway:
    repository: registry.example.com/messenger/gateway
    tag: consumer-release
  worker:
    repository: registry.example.com/messenger/worker
    tag: consumer-release
```

For immutable application pins, set `digest` to the digest returned by the
consumer's publication process for **each** image. A structural example is:

```yaml
images:
  gateway:
    digest: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

Label that digest synthetic and non-deployable; do not imply all images share it.
Postgres/Redis accept their own consumer-approved complete digest references.

Document safe offline usage with a private consumer values file supplied by
the operator, and no credentials on the command line:

```powershell
$ConsumerValues = Read-Host 'Absolute consumer values file (references only, no inline credentials)'
if (-not [IO.Path]::IsPathFullyQualified($ConsumerValues) -or
    -not (Test-Path $ConsumerValues -PathType Leaf)) {
    throw 'Supply an existing absolute consumer values file'
}
helm lint .\deploy\helm\push-messenger --strict -f $ConsumerValues
if ($LASTEXITCODE -ne 0) { throw 'Consumer chart lint failed' }
helm template messenger .\deploy\helm\push-messenger --namespace messenger -f $ConsumerValues |
    Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Consumer offline render failed' }
```

Explain that references-only external mode emits no application Secret;
chart-managed rendering **does** emit credentials and must not go to shared
logs/artifacts. The consumer materializes both Secrets before workload startup.
Missing required external keys produce Kubernetes `CreateContainerConfigError`;
invalid formats/credentials fail application startup/readiness. Helm cannot
prove contents offline and must not silently fall back.

State annotation precedence, optional class, all-route TLS, public identity when
Ingress is disabled, and consumer-supplied Gateway routes. Explain that
`clientIpHeader: ""` avoids trusting client headers but counts proxy peers when
behind a controller; it is not a production per-client rate-limiting setup.
Require forwarding-header sanitization, trusted proxy hops and restricted direct
backend access before selecting a header.

- [ ] **Step 2: Add a credential-source transition subsection to the new guide.**

The subsection must specify this ordered procedure, not a new provisioning tool:

1. Complete/rehearse the 11A upgrade first if needed; snapshot chart/images,
   credential-source references and backed-up data privately.
2. Under consumer maintenance, securely copy the **unchanged** required values
   into a distinctly named consumer-owned Secret in the same namespace; verify
   password/URL agreement privately. Do not generate new passwords/keys as a
   side effect of source selection. Do not use `--set` or log `helm get all`.
3. Set `secrets.existingSecret`, remove all nine inline credential settings,
   explicitly preserve webhook enabled/disabled state, select controller/TLS
   settings, and deploy coordinated images/chart. The old kept chart Secret is
   retained, not adopted as a new externally owned object or deleted.
4. Verify readiness, existing data and normal application traffic before
   reopening maintenance. With consumer routes, maintenance must independently
   gate those routes; setting `ingress.enabled=false` is not a global traffic gate.
5. For rollback without changed credentials, restore the previous matching
   chart/values/images and its retained Secret under maintenance. If backing
   credentials changed, coordinate their restoration too; Helm rollback alone
   is insufficient. Keep both sources until rollback is explicitly retired.

Say that changing external Secret content does not automatically roll workloads
or rotate database roles. Consumer coordination/rollout is mandatory. Never
change the external Secret's Helm labels/annotations, delete it on uninstall,
or put a full Secret manifest/credential fixture in this repository.

- [ ] **Step 3: Update `NOTES.txt` ownership statements.**

Replace its unconditional kept-Secret paragraph with:

```gotemplate
{{- if .Values.secrets.existingSecret }}
Credentials reference consumer-managed Secret "{{ include "push.secretName" . }}".
The chart does not create, adopt, update or delete it. Provision it before startup.
{{- else }}
The chart-managed Secret "{{ include "push.secretName" . }}" and PostgreSQL volume
are retained on uninstall; keep credentials consistent with the retained database.
{{- end }}
{{- if and .Values.ingress.enabled (eq .Values.ingress.scheme "https") }}
Public Ingress requires existing TLS Secret "{{ .Values.ingress.tls.secretName }}".
The consumer supplies the controller, certificate and enforcing network policies.
{{- end }}
```

Move destructive local cleanup advice inside the DEV ONLY section, with an
explicit data-loss warning; do not suggest `down` to a consumer upgrading data.
If Ingress is disabled, describe URLs as configured public identities whose
routes the consumer must supply, not as proof the endpoints are reachable.

- [ ] **Step 4: Update directly related documentation/comments.**

In `README.md`, link the configuration guide and maintained-controller guide.
Describe nginx as the retired local compatibility path, not a production
recommendation; replace the "same chart that will deploy to Azure" claim with
the cloud-neutral consumer chart boundary. Keep existing local commands.

In `deployment-upgrade.md`, preserve the tested 11A commands and add a clear
scope note: they describe the historical chart-managed transition; operators
moving to the 11B chart must additionally set explicit webhook/controller/TLS
values and follow the new credential-source guide. Do not rewrite the private
named-pipe function or suggest `--patch-file=-`.

In `values.yaml` and `secret.yaml`, remove stale promises that an Azure/Key Vault
plan will supply credentials/TLS. State that consumers deliver them. Keep the
existing sender/from defaults; changing branding is outside this task.

- [ ] **Step 5: Commit the contract documentation and NOTES change.**

```powershell
pwsh .\deploy\helm\test-chart.ps1
git add docs\deployment-configuration.md docs\deployment-upgrade.md README.md deploy\helm\push-messenger\templates\NOTES.txt deploy\helm\push-messenger\values.yaml deploy\helm\push-messenger\templates\secret.yaml
git commit -m "docs: define consumer deployment configuration and transitions" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 7: Verify fresh local compatibility and maintained-controller behavior

**Deliverable:** `docs\deployment-ingress-verification.md`, containing commands,
test fixtures and an honest results table for the actual execution. Do not add a
production probe or load harness here. Any temporary manifests/credential
material belong in a private encrypted operator directory outside Git.

- [ ] **Step 1: Re-run the existing local path on a new owned enforcing-CNI profile.**

```powershell
minikube start -p push11b-local --driver=docker --cni=calico --cpus=4 --memory=6g
if ($LASTEXITCODE -ne 0) { throw 'Local verification profile failed' }
.\deploy\minikube\up.ps1 -Cluster push11b-local -Smoke
if ($LASTEXITCODE -ne 0) { throw 'Existing local smoke failed' }
.\deploy\minikube\test-network-policy.ps1 -Context push11b-local
if ($LASTEXITCODE -ne 0) { throw 'Local network isolation failed' }
```

Expected: raw protocol and SDK smoke pass, including local generated credentials,
log-only mailbox and webhook route; all real API allow/deny controls pass. Record
versions and exit results, not credential-bearing manifests.

- [ ] **Step 2: Prepare a separate Traefik verification installation.**

Create only the new owned cluster:

```powershell
minikube start -p push11b-traefik --driver=docker --cni=calico --cpus=4 --memory=6g
if ($LASTEXITCODE -ne 0) { throw 'Traefik verification profile failed' }
```

Do **not** invoke `up` here: it installs nginx. Build/load the same five local images with the existing Dockerfile:

```powershell
$root = (Get-Location).Path # run from the checked-out repository root
$dockerVariables = 'DOCKER_TLS_VERIFY', 'DOCKER_HOST', 'DOCKER_CERT_PATH', 'MINIKUBE_ACTIVE_DOCKERD'
$saved = @{}
foreach ($name in $dockerVariables) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name)
}
try {
    $dockerEnv = minikube -p push11b-traefik docker-env --shell powershell
    if ($LASTEXITCODE -ne 0) { throw 'Verification Docker environment failed' }
    $dockerEnv | Out-String | Invoke-Expression
    foreach ($target in 'auth', 'message-svc', 'gateway', 'worker', 'smoke') {
        docker build -f .\deploy\docker\Dockerfile --target $target `
            -t "push-messenger/${target}:dev" $root
        if ($LASTEXITCODE -ne 0) { throw "Verification image build failed: $target" }
    }
} finally {
    foreach ($name in $dockerVariables) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name])
    }
}
```

Use Traefik Helm chart **42.0.0**, whose pinned `Chart.yaml` specifies
**v3.7.14** and Kubernetes >=1.30. It was published 2026-10-15. Before executing
this task, check upstream maintenance/security notices; if this pin becomes
unsupported or affected, stop and review an explicit replacement, never silently
install `latest`. Record the actual chart checksum and running image ID.

The verification-only Traefik values file must contain:

```yaml
ingressClass:
  enabled: true
  isDefaultClass: false
  name: traefik
providers:
  kubernetesCRD:
    enabled: false
  kubernetesGateway:
    enabled: false
  kubernetesIngress:
    enabled: true
    ingressClass: traefik
    allowEmptyServices: true
api:
  dashboard: false
service:
  type: NodePort
ports:
  web:
    forwardedHeaders:
      insecure: false
      trustedIPs: []
  websecure:
    forwardedHeaders:
      insecure: false
      trustedIPs: []
    transport:
      respondingTimeouts:
        readTimeout: 120s
        writeTimeout: 0s
        idleTimeout: 120s
log:
  level: ERROR
  format: json
accessLog:
  enabled: true
  format: json
  bufferingSize: 0
  fields:
    defaultMode: drop
    names:
      RequestMethod: keep
      DownstreamStatus: keep
      RouterName: keep
      Duration: keep
    headers:
      defaultMode: drop
    queryParameters:
      defaultMode: drop
```

Use the pinned chart values rather than old `logs.access` examples: chart 42
uses top-level `log` and `accessLog`. Allow-listing access-log fields excludes
paths/query/referer/authorization; ERROR logs still require empirical checks.
Never enable forwarding-header `insecure`, debug logs or arbitrary snippets.
Disabling Kubernetes CRD providers does not require the application chart to ship
CRDs. Install the verification controller with Helm's `--skip-crds`:

```powershell
$TraefikValues = Read-Host 'Absolute verification-only Traefik values file'
helm repo add traefik https://traefik.github.io/charts
if ($LASTEXITCODE -ne 0) { throw 'Controller chart repository failed' }
helm upgrade --install edge traefik/traefik --version 42.0.0 --skip-crds `
    --kube-context push11b-traefik -n edge --create-namespace `
    -f $TraefikValues --wait --timeout 5m
if ($LASTEXITCODE -ne 0) { throw 'Pinned verification controller failed' }
```

Controller installation is **only** in this owned verification cluster. Do not
add it as an application subchart or a cluster-wide installer for consumers.

- [ ] **Step 3: Provision private fixture credentials and a trusted test certificate.**

Include this complete PowerShell 7 fixture. Run without tracing/transcription;
the helper captures rejected manifests/errors privately in memory and emits no
response bodies. It is an acceptance fixture, not consumer Secret delivery:

```powershell
kubectl --context push11b-traefik create namespace push
if ($LASTEXITCODE -ne 0) { throw 'Create the fresh verification namespace first' }
$Private = Read-Host 'Existing PRIVATE ENCRYPTED directory outside Git for local fixture files'
if (-not [IO.Path]::IsPathFullyQualified($Private) -or
    -not (Test-Path $Private -PathType Container)) {
    throw 'Prepare and verify the private encrypted directory first'
}
function New-FixturePassword {
    [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(24))
}
function Invoke-FixtureCreate([object] $Manifest) {
    $info = [Diagnostics.ProcessStartInfo]::new('kubectl')
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($arg in @('--context', 'push11b-traefik', '-n', 'push', 'create',
        '--request-timeout=30s', '-f', '-', '-o', 'name')) {
        $info.ArgumentList.Add($arg)
    }
    $child = [Diagnostics.Process]::Start($info)
    $stdout = $child.StandardOutput.ReadToEndAsync()
    $stderr = $child.StandardError.ReadToEndAsync()
    try {
        $payload = $Manifest | ConvertTo-Json -Depth 24 -Compress
        if ($payload.Length -gt 65536) { throw 'Unexpectedly large verification manifest' }
        $child.StandardInput.WriteLine($payload)
        $child.StandardInput.Close()
        if (-not $child.WaitForExit(45000)) { throw 'Verification create timed out' }
        if ($child.ExitCode -ne 0) {
            throw 'Verification create failed; diagnostic bodies withheld'
        }
    } finally {
        if (-not $child.HasExited) { $child.Kill($true); $child.WaitForExit() }
        $child.Dispose()
        $payload = $stdout = $stderr = $null
    }
}
$admin = New-FixturePassword
$messaging = New-FixturePassword
$auth = New-FixturePassword
$worker = New-FixturePassword
$redis = New-FixturePassword
$FixtureSecret = @{
    apiVersion = 'v1'; kind = 'Secret'
    metadata = @{ name = 'consumer-credentials'; namespace = 'push' }
    type = 'Opaque'
    stringData = @{
        'postgres-password' = $admin
        'messaging-db-password' = $messaging
        'database-url' = "postgres://push_messaging:${messaging}@push-postgres:5432/push_messaging"
        'auth-db-password' = $auth
        'auth-database-url' = "postgres://push_auth:${auth}@push-postgres:5432/push_auth"
        'worker-db-password' = $worker
        'worker-database-url' = "postgres://push_worker:${worker}@push-postgres:5432/push_worker"
        'worker-auth-database-url' = "postgres://push_worker:${worker}@push-postgres:5432/push_auth"
        'redis-password' = $redis
        'redis-url' = "redis://:${redis}@push-redis:6379"
        'auth-signing-key' = [Convert]::ToBase64String(
            [Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
        'auth-link-key' = [Convert]::ToBase64String(
            [Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
        'accounts-token' = New-FixturePassword
        'directory-token' = New-FixturePassword
        'messaging-token' = New-FixturePassword
        'resend-webhook-secret' = 'whsec_' + [Convert]::ToBase64String(
            [Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
    }
}
Invoke-FixtureCreate $FixtureSecret
$FixtureSecret = $admin = $messaging = $auth = $worker = $redis = $null

$rsa = [Security.Cryptography.RSA]::Create(2048)
$request = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
    'CN=push11b.test', $rsa, [Security.Cryptography.HashAlgorithmName]::SHA256,
    [Security.Cryptography.RSASignaturePadding]::Pkcs1)
$san = [Security.Cryptography.X509Certificates.SubjectAlternativeNameBuilder]::new()
$san.AddDnsName('push11b.test')
$request.CertificateExtensions.Add($san.Build())
# Test-only self-signed trust anchor; not a production issuing CA.
$request.CertificateExtensions.Add(
    [Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new(
        $true, $false, 0, $true))
$cert = $request.CreateSelfSigned([DateTimeOffset]::UtcNow.AddMinutes(-5),
    [DateTimeOffset]::UtcNow.AddDays(2))
try {
    $pem = $cert.ExportCertificatePem()
    Invoke-FixtureCreate @{
        apiVersion = 'v1'; kind = 'Secret'
        metadata = @{ name = 'push-tls'; namespace = 'push' }
        type = 'kubernetes.io/tls'
        stringData = @{ 'tls.crt' = $pem; 'tls.key' = $rsa.ExportPkcs8PrivateKeyPem() }
    }
    [IO.File]::WriteAllText((Join-Path $Private 'push11b-ca.pem'), $pem)
} finally {
    $cert.Dispose()
    $rsa.Dispose()
    $pem = $null
}
```

Only the public certificate is written to the private directory; the fixture
private key stays in memory and the Kubernetes Secret. Do not print it, add it
to a user/machine trust store or save it in a kubeconfig. No cert-manager, ACME,
DNS or consumer certificate provisioner is installed by the application chart.

The reference-only application fixture must use:

```yaml
secrets:
  existingSecret: consumer-credentials
devMode:
  enabled: true
  acknowledge: insecure-local-only
worker:
  replicas: 1
  email:
    sender: log
    webhook:
      enabled: true
ingress:
  enabled: true
  className: traefik
  host: push11b.test
  scheme: https
  tls:
    secretName: push-tls
  annotations:
    traefik.ingress.kubernetes.io/router.entrypoints: websecure
auth:
  clientIpHeader: x-forwarded-for
gateway:
  clientIpHeader: x-forwarded-for
networkPolicy:
  ingressControllerPeers:
    - namespaceSelector:
        kubernetes.io/metadata.name: edge
      podSelector:
        app.kubernetes.io/name: traefik
```

This is a DEV ONLY fixture, not production values. Inspect actual controller
labels before using those selectors. Install with an explicit context/namespace:

```powershell
$ApplicationValues = Read-Host 'Absolute reference-only application fixture values'
helm upgrade --install push .\deploy\helm\push-messenger `
    --kube-context push11b-traefik -n push -f $ApplicationValues --wait --timeout 10m
if ($LASTEXITCODE -ne 0) { throw 'External-Secret fixture failed' }
.\deploy\minikube\test-network-policy.ps1 -Context push11b-traefik
if ($LASTEXITCODE -ne 0) { throw 'Traefik fixture internal isolation failed' }
```

Confirm Helm's manifest contains no Secret, and record the external Secret UID,
resourceVersion and ownership metadata before/after upgrade. Compare in memory,
print only equality results. Uninstall a separate disposable **application
release** using its own namespace/Secret and verify that Secret remains with the
same ownership/UID. Do not uninstall the data-bearing acceptance release.

- [ ] **Step 4: Write/run bounded TLS and WebSocket checks against real routing.**

Include the following local-only Python fixture in the verification document
as `check-ingress.py` (operator scratch, not a new production tool). It uses only
the Python standard library. Its acceptance requirements are:

1. Connect TCP to the explicit Traefik service endpoint, but use TLS SNI and HTTP
   Host `push11b.test`. Use `ssl.create_default_context(cafile=...)`; never
   `CERT_NONE`, `check_hostname=False`, curl `-k` or disabled SDK validation.
2. Prove valid CA/hostname succeeds on `/v1/auth/jwks`, unrelated CA fails,
   and wrong TLS hostname fails. A Host override alone does not set TLS SNI.
3. Enroll two local test users with the **existing** signup -> mailbox
   `verify_email` -> verify -> login -> `POST /v1/devices` flow implemented in
   `crates\push-smoke\src\auth.rs:100-220`. Keep credentials/tokens in memory.
   Access `/dev/mailbox` only through an operator-only worker port-forward or
   same-release smoke-labeled fixture pod; prove the public route does not expose
   it. Check HTTP status before parsing; never print token-bearing response bodies.
4. Complete a TLS WebSocket HTTP/1.1 upgrade on `/v1/socket`; verify status 101
   and `Sec-WebSocket-Accept`, send masked binary protobuf Identify, wait for
   Ready, and then send application Ping/expect Pong every announced heartbeat.
   Use application Ping/Pong, not WebSocket ping as a substitute:

```python
import argparse
import base64
import hashlib
import http.client
import json
import os
import socket
import ssl
import struct
import time
import urllib.parse
import uuid

parser = argparse.ArgumentParser()
parser.add_argument("--connect-host", required=True)
parser.add_argument("--connect-port", required=True, type=int)
parser.add_argument("--ca", required=True)
parser.add_argument("--mailbox-port", type=int, default=18081)
parser.add_argument("--canary", required=True)
parser.add_argument("--links-only", action="store_true")
parser.add_argument("--link-status", type=int, default=200)
args = parser.parse_args()
HOST = "push11b.test"
context = ssl.create_default_context(cafile=args.ca)


def tls(ctx=context, name=HOST):
    raw = socket.create_connection((args.connect_host, args.connect_port), timeout=10)
    try:
        return ctx.wrap_socket(raw, server_hostname=name)
    except BaseException:
        raw.close()
        raise


def request(method, path, body=None, bearer=None, forged=None, mailbox=False):
    conn = http.client.HTTPConnection(
        "127.0.0.1" if mailbox else args.connect_host,
        args.mailbox_port if mailbox else args.connect_port, timeout=15)
    if not mailbox:
        conn.sock = tls()
    headers = {"Host": HOST}
    if bearer:
        headers["Authorization"] = "Bearer " + bearer
    if forged:
        headers.update({"X-Forwarded-For": forged, "X-Real-IP": forged})
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    try:
        conn.request(method, path, data, headers)
        response = conn.getresponse()
        payload = response.read(1024 * 1024 + 1)
        assert len(payload) <= 1024 * 1024
        return response.status, dict(response.getheaders()), payload
    finally:
        conn.close()


def post(path, body, expected, bearer=None, forged=None):
    status, _, payload = request("POST", path, body, bearer, forged)
    assert status == expected, "unexpected API status"
    return json.loads(payload) if payload else {}


def enroll():
    suffix = uuid.uuid4().hex[:10]
    email, password = suffix + "@ingress.test", uuid.uuid4().hex + "Az!7"
    post("/v1/auth/signup", {"email": email, "password": password,
         "handle": "t_" + suffix, "display_name": "Ingress fixture"}, 202)
    deadline, token = time.monotonic() + 30, None
    while time.monotonic() < deadline:
        status, _, payload = request("GET", "/dev/mailbox?to=" +
            urllib.parse.quote(email), mailbox=True)
        assert status == 200
        emails = json.loads(payload).get("emails", [])
        for sent in emails:
            if sent["template"] == "verify_email":
                link = next(line for line in sent["text"].splitlines()
                            if line.startswith("http"))
                token = urllib.parse.parse_qs(
                    urllib.parse.urlsplit(link).query)["token"][0]
        if token:
            break
        time.sleep(0.25)
    assert token, "local verification email not delivered"
    post("/v1/auth/verify", {"token": token}, 200)
    login = post("/v1/auth/login", {"email": email, "password": password}, 200)
    registered = post("/v1/devices", {"name": "Ingress fixture", "platform": "other",
        "registration_id": str(uuid.uuid4())}, 201, login["registration_token"])
    return registered["access_token"]


def varint(value):
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def blob(number, data):
    return varint(number * 8 + 2) + varint(len(data)) + data


def read_varint(data, offset):
    value = 0
    for shift in range(0, 70, 7):
        byte = data[offset]
        offset += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, offset
    raise AssertionError("oversized protobuf varint")


def fields(data):
    offset, result = 0, {}
    while offset < len(data):
        tag, offset = read_varint(data, offset)
        number, wire = tag >> 3, tag & 7
        assert number > 0
        if wire == 0:
            value, offset = read_varint(data, offset)
        elif wire == 2:
            size, offset = read_varint(data, offset)
            assert offset + size <= len(data)
            value, offset = data[offset:offset + size], offset + size
        else:
            raise AssertionError("unexpected fixture protobuf wire type")
        result[number] = value
    return result


def exact(conn, size):
    result = bytearray()
    while len(result) < size:
        part = conn.recv(size - len(result))
        assert part, "unexpected EOF"
        result.extend(part)
    return bytes(result)


def send(conn, opcode, payload):
    mask = os.urandom(4)
    size = len(payload)
    assert size <= 65535
    header = (bytes([128 | opcode, 128 | size]) if size < 126 else
              bytes([128 | opcode, 254]) + struct.pack("!H", size))
    conn.sendall(header + mask + bytes(b ^ mask[i % 4]
                                     for i, b in enumerate(payload)))


def receive(conn):
    fragments = bytearray()
    started = False
    while True:
        a, b = exact(conn, 2)
        assert not (a & 112) and not (b & 128), "unexpected extension/server mask"
        opcode, size = a & 15, b & 127
        if size == 126:
            size = struct.unpack("!H", exact(conn, 2))[0]
        elif size == 127:
            size = struct.unpack("!Q", exact(conn, 8))[0]
        assert size <= 65536 and len(fragments) + size <= 65536
        payload = exact(conn, size)
        if opcode == 8:
            return "close", struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else 1005
        if opcode == 9:
            send(conn, 10, payload)
            continue
        if opcode == 10:
            continue
        assert opcode == (0 if started else 2), "expected binary protobuf"
        started = True
        fragments.extend(payload)
        if a & 128:
            return "data", bytes(fragments)


def connect(token, forged):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        conn = tls()
        try:
            key = base64.b64encode(os.urandom(16)).decode()
            headers = ("GET /v1/socket HTTP/1.1\r\nHost: " + HOST +
                "\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " +
                key + "\r\nSec-WebSocket-Version: 13\r\nX-Forwarded-For: " + forged +
                "\r\nX-Real-IP: " + forged + "\r\n\r\n")
            conn.sendall(headers.encode("ascii"))
            response = bytearray()
            while not response.endswith(b"\r\n\r\n"):
                assert len(response) < 16384
                response.extend(exact(conn, 1))
            lines = response.decode("ascii").split("\r\n")
            assert lines[0].split()[1] == "101"
            returned = dict(line.split(":", 1) for line in lines[1:] if ":" in line)
            returned = {k.lower(): v.strip() for k, v in returned.items()}
            accept = base64.b64encode(hashlib.sha1((key +
                "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            assert returned["sec-websocket-accept"] == accept
            send(conn, 2, blob(1, blob(1, token.encode("ascii"))))
            kind, answer = receive(conn)
            if kind == "close" and answer == 4006:
                conn.close()
                time.sleep(1)
                continue
            assert kind == "data" and 32 in fields(answer), "Identify did not produce Ready"
            heartbeat = fields(fields(answer)[32])[3] / 1000
            assert heartbeat == 30, "fixture expects default heartbeat"
            return conn, heartbeat
        except BaseException:
            conn.close()
            raise
    raise AssertionError("device relay not ready within deadline")


def close(conn):
    try:
        send(conn, 8, struct.pack("!H", 1000))
    except OSError:
        pass  # Closing a fixture after a peer close needs no successful write.
    finally:
        conn.close()


APPLICATION_PING = b"\x22\x00"  # Frame.ping = 4, empty Ping.
APPLICATION_PONG = b"\xa2\x02\x00"  # Frame.pong = 36, empty Pong.


def main():
    assert args.canary.startswith("push11b-canary-")
    for path in ("/v1/auth/verify", "/v1/auth/password/reset"):
        status, _, _ = request("GET", path + "?token=" +
                               urllib.parse.quote(args.canary))
        assert status == args.link_status, "unexpected link-route status"
        print("ok   link route status", status)
    if args.links_only:
        return
    with tls():
        pass
    for ctx, name in ((ssl.create_default_context(), HOST), (context, "wrong.test")):
        try:
            with tls(ctx, name):
                raise AssertionError("invalid TLS trust/name accepted")
        except ssl.SSLCertVerificationError:
            pass
    assert request("GET", "/v1/auth/jwks")[0] == 200
    assert request("GET", "/dev/mailbox")[0] == 404
    tokens = [enroll(), enroll()]
    conn, heartbeat = connect(tokens[0], "198.51.100.10")
    active_started = time.monotonic()
    try:
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline:
            time.sleep(min(heartbeat, max(0, deadline - time.monotonic())))
            send(conn, 2, APPLICATION_PING)
            assert receive(conn) == ("data", APPLICATION_PONG), "application Pong missing"
        active_elapsed = time.monotonic() - active_started
    finally:
        close(conn)
    conn, heartbeat = connect(tokens[1], "198.51.100.11")
    try:
        conn.settimeout(100)
        started = time.monotonic()
        assert receive(conn) == ("close", 1008), "not the application idle close"
        elapsed = time.monotonic() - started
        assert heartbeat * 2 <= elapsed <= heartbeat * 3 + 10
    finally:
        close(conn)
    # Exercise each gateway's IP selection without exhausting a per-user quota.
    for index, token in enumerate(tokens):
        conn, _ = connect(token, "198.51.100." + str(10 + index))
        try:
            redeem = blob(1, b"ZZZZZZZZZZ")
            request_body = b"\x08\x01" + blob(12, redeem)
            send(conn, 2, blob(9, request_body))
            kind, payload = receive(conn)
            assert kind == "data"
            error = fields(fields(payload)[35])
            assert error[1] == 18 and error[5] == 1, "expected invite-not-found"
        finally:
            close(conn)
    unknown = uuid.uuid4().hex + "@negative.test"
    for index in range(11):
        status, headers, _ = request("POST", "/v1/auth/login",
            {"email": unknown, "password": "invalid-fixture-password"},
            forged="198.51.100." + str(30 + index))
        assert status == (401 if index < 10 else 429), "spoofed login quota identity"
        if status == 429:
            assert int(next(v for k, v in headers.items()
                            if k.lower() == "retry-after")) > 0
    print("ok   TLS trust/name, WSS, heartbeat and auth spoof guard")
    print("ok   active socket seconds", round(active_elapsed, 1))
    print("ok   idle socket seconds", round(elapsed, 1), "close", 1008)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Never echo request URLs, bodies, headers or exception messages.
        print("FAIL ingress fixture:", type(error).__name__)
        raise SystemExit(1) from None
```

The authority for fixture fields is
`crates\push-proto\proto\push\v1\frame.proto`. Keep its literals local to this
verification fixture, not a new supported client implementation. Run it as a
bounded process (480-second overall deadline) and kill only that captured child
on timeout. Fix a fixture assertion failure; never weaken TLS validation or mark
a failed invocation passed.

Before invoking, open an owned loopback-only worker mailbox port-forward and
probe its readiness:

```powershell
$MailboxForward = Start-Process kubectl -NoNewWindow -PassThru -ArgumentList @(
    '--context', 'push11b-traefik', '-n', 'push', 'port-forward',
    'svc/push-worker', '18081:8080', '--address', '127.0.0.1')
$mailboxReady = $false
for ($attempt = 0; $attempt -lt 20; $attempt++) {
    if ($MailboxForward.HasExited) { throw 'Owned mailbox port-forward exited' }
    $probe = [Net.Sockets.TcpClient]::new()
    try {
        $connected = $probe.ConnectAsync('127.0.0.1', 18081)
        if ($connected.Wait(1000) -and $probe.Connected) { $mailboxReady = $true; break }
    } catch [AggregateException] {
        # Bounded startup retry for the not-yet-listening owned tunnel.
    } finally {
        $probe.Dispose()
    }
    Start-Sleep -Seconds 1
}
if (-not $mailboxReady) { throw 'Owned mailbox tunnel not responsive' }
```

Use the minikube node's explicit IP and the controller Service's HTTPS NodePort
for the public path, not the mailbox tunnel:

```powershell
$ConnectHost = minikube ip -p push11b-traefik
if ($LASTEXITCODE -ne 0) { throw 'Verification node address unavailable' }
$ConnectPort = kubectl --context push11b-traefik -n edge get service edge-traefik `
    -o 'jsonpath={.spec.ports[?(@.name=="websecure")].nodePort}'
if ($LASTEXITCODE -ne 0 -or -not $ConnectPort) { throw 'HTTPS NodePort unavailable' }
$Canary = 'push11b-canary-' + [guid]::NewGuid().ToString('N')
$RedisPod = kubectl --context push11b-traefik -n push get pod `
    -l 'app.kubernetes.io/name=redis,app.kubernetes.io/instance=push' `
    -o 'jsonpath={.items[0].metadata.name}'
if ($LASTEXITCODE -ne 0 -or -not $RedisPod) { throw 'Fixture Redis pod unavailable' }
function Get-FixtureRedeemKeys {
    $keys = @(kubectl --context push11b-traefik -n push exec $RedisPod -- sh -c `
        'export REDISCLI_AUTH="$REDIS_PASSWORD"; exec redis-cli --scan --pattern "gw:redeem:ip:*"')
    if ($LASTEXITCODE -ne 0) { throw 'Read fixture rate-limit key inventory failed' }
    @($keys | Where-Object { $_ })
}
function Invoke-IngressFixture([int] $LinkStatus = 200, [switch] $LinksOnly) {
    $info = [Diagnostics.ProcessStartInfo]::new('python')
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($arg in @((Join-Path $Private 'check-ingress.py'),
        '--connect-host', "$ConnectHost", '--connect-port', "$ConnectPort",
        '--ca', (Join-Path $Private 'push11b-ca.pem'), '--canary', $Canary,
        '--link-status', "$LinkStatus")) {
        $info.ArgumentList.Add($arg)
    }
    if ($LinksOnly) { $info.ArgumentList.Add('--links-only') }
    $child = [Diagnostics.Process]::Start($info)
    $stdout = $child.StandardOutput.ReadToEndAsync()
    $stderr = $child.StandardError.ReadToEndAsync()
    try {
        if (-not $child.WaitForExit(480000)) { throw 'Ingress fixture timed out' }
        if ($child.ExitCode -ne 0) { throw 'Ingress fixture failed; diagnostic bodies withheld' }
        Write-Host ($stdout.GetAwaiter().GetResult())
    } finally {
        if (-not $child.HasExited) { $child.Kill($true); $child.WaitForExit() }
        $child.Dispose()
        $stdout = $stderr = $null
    }
}
if (@(Get-FixtureRedeemKeys).Count -ne 0) {
    throw 'This quota proof requires a fresh fixture; do not flush Redis'
}
Invoke-IngressFixture
```

If the node IP is not reachable from Windows with this driver, use an explicitly
owned TLS TCP port-forward for these checks and mark source-preservation evidence
unavailable. Do not claim NodePort/LB acceptance from a tunnel.

Keep one authenticated socket alive for **180 seconds**, longer than 120 seconds,
with regular application Ping/Pong. Then open a distinct registered device,
Identify, send no frames and require gateway policy close **1008** after about
2.5 heartbeats (75 seconds at the default), before a generous 100-second
deadline. Record elapsed time and close code. An earlier proxy EOF, timeout or
missing Pong fails the run; do not relabel it as expected idle behavior.

This fixture is a small local routing test. Do not add account discovery,
production mailbox access, signing-key dependencies or a capacity claim.
The general authorized-target harness remains 11C.

- [ ] **Step 5: Prove token-safe logging on both link routes during upstream failures.**

Use only a synthetic marker, never a real email token:

```powershell
$Canary = 'push11b-canary-' + [guid]::NewGuid().ToString('N')
$Since = (Get-Date).ToUniversalTime().ToString('o')
```

For both `/v1/auth/verify?token=<canary>` and
`/v1/auth/password/reset?token=<canary>`, test:

| State | Exact action | Required evidence |
|---|---|---|
| Healthy upstream | TLS GET each URL | Auth-owned link response; no query marker in controller/application logs |
| No endpoints | Scale only `push-auth` to zero; wait for endpoints to disappear | 503 with `allowEmptyServices`; nonzero error-log/access-log activity or an explicit status entry, no marker |
| Connection refused | Point the fixture auth Service's target port to unused 65530, with auth running | 502/504-class proxy failure, no marker in access/error logs |
| Read timeout | Scale auth to zero and run a uniquely named, separately owned fixture Job with matching auth Service labels; withhold headers past the configured upstream response-header timeout | Actual 504 after the configured deadline, no marker |

Set the verification controller's upstream response-header timeout explicitly
to 5 seconds for the timeout test:

```yaml
additionalArguments:
  - --serverstransport.forwardingtimeouts.responseheadertimeout=5s
```

For the timeout backend, a `python:3.13-alpine` fixture uses this handler. Give
the fixture Job a 90-second active deadline and delete its named Job in
`finally`; never attach these labels to an unrelated/shared workload:

```python
from http.server import BaseHTTPRequestHandler, HTTPServer
import time


class Stall(BaseHTTPRequestHandler):
    def do_GET(self):
        time.sleep(15)
        self.send_response(503)
        self.end_headers()

    def log_message(self, *args):
        pass


HTTPServer(("0.0.0.0", 8080), Stall).serve_forever()
```

The fixture Job has its own selector/owner but adds the existing auth
Service's two selector labels to its Pod. With real auth scaled to zero, the
unchanged Service selects only the fixture backend. This leaves all chart
Ingress resources intact and tests the real Exact link routes. Restore the
original Service target port and auth replicas in `finally`; do not patch a
Helm-managed route and assume Helm cannot replace it. Keep this exercise confined
to the isolated fixture and a finite test window. Execute the following, using
the already defined private-create and ingress-fixture helpers:

```powershell
function NativeOK([string] $Operation) {
    if ($LASTEXITCODE -ne 0) { throw "$Operation failed" }
}
function Wait-AuthEndpoints([int] $Port) {
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        $json = kubectl --context push11b-traefik -n push get endpointslice `
            -l 'kubernetes.io/service-name=push-auth' -o json
        NativeOK 'Read fixture auth EndpointSlices'
        $ready = @(($json | ConvertFrom-Json).items | Where-Object {
            @($_.endpoints | Where-Object { $_.conditions.ready -ne $false }).Count -gt 0
        })
        if ($Port -eq 0 -and $ready.Count -eq 0) { return }
        $ports = @($ready | ForEach-Object { $_.ports } | ForEach-Object { $_.port })
        if ($Port -ne 0 -and $ready.Count -gt 0 -and $ports.Count -gt 0 -and
            @($ports | Where-Object { $_ -ne $Port }).Count -eq 0) { return }
        Start-Sleep -Seconds 2
    }
    throw 'Fixture auth endpoints did not converge'
}
$AuthReplicas = kubectl --context push11b-traefik -n push get deployment push-auth `
    -o 'jsonpath={.spec.replicas}'
NativeOK 'Record fixture auth replicas'
$StallName = 'push11b-stall-' + [guid]::NewGuid().ToString('N').Substring(0, 10)
$StallCode = @'
from http.server import BaseHTTPRequestHandler, HTTPServer
import time
class Stall(BaseHTTPRequestHandler):
    def do_GET(self):
        time.sleep(15)
        self.send_response(503)
        self.end_headers()
    def log_message(self, *args):
        pass
HTTPServer(("0.0.0.0", 8080), Stall).serve_forever()
'@
try {
    kubectl --context push11b-traefik -n push scale deployment push-auth --replicas=0
    NativeOK 'Stop fixture auth'
    kubectl --context push11b-traefik -n push wait --for=delete pod `
        -l 'app.kubernetes.io/name=auth,app.kubernetes.io/instance=push' --timeout=120s
    NativeOK 'Wait for fixture auth pods to stop'
    Wait-AuthEndpoints 0
    Invoke-IngressFixture -LinksOnly -LinkStatus 503

    kubectl --context push11b-traefik -n push scale deployment push-auth --replicas=$AuthReplicas
    NativeOK 'Restore fixture auth for refusal test'
    kubectl --context push11b-traefik -n push rollout status deployment/push-auth --timeout=180s
    NativeOK 'Ready fixture auth for refusal test'
    kubectl --context push11b-traefik -n push patch service push-auth --type=json `
        --patch '[{"op":"replace","path":"/spec/ports/0/targetPort","value":65530}]'
    NativeOK 'Use unused fixture upstream port'
    Wait-AuthEndpoints 65530
    Invoke-IngressFixture -LinksOnly -LinkStatus 502
    kubectl --context push11b-traefik -n push patch service push-auth --type=json `
        --patch '[{"op":"replace","path":"/spec/ports/0/targetPort","value":"http"}]'
    NativeOK 'Restore named upstream port'
    kubectl --context push11b-traefik -n push scale deployment push-auth --replicas=0
    NativeOK 'Stop auth for controlled timeout backend'
    kubectl --context push11b-traefik -n push wait --for=delete pod `
        -l 'app.kubernetes.io/name=auth,app.kubernetes.io/instance=push' --timeout=120s
    NativeOK 'Wait before creating timeout fixture'
    $labels = @{
        'app.kubernetes.io/name' = 'auth'
        'app.kubernetes.io/instance' = 'push'
        'push-messenger/network-peer' = 'auth'
        'push-messenger/verification-fixture' = $StallName
    }
    Invoke-FixtureCreate @{
        apiVersion = 'batch/v1'; kind = 'Job'
        metadata = @{ name = $StallName; namespace = 'push' }
        spec = @{
            activeDeadlineSeconds = 90
            backoffLimit = 0
            template = @{
                metadata = @{ labels = $labels }
                spec = @{
                    automountServiceAccountToken = $false
                    restartPolicy = 'Never'
                    securityContext = @{
                        runAsNonRoot = $true; runAsUser = 65532
                        seccompProfile = @{ type = 'RuntimeDefault' }
                    }
                    containers = @(@{
                        name = 'stall'; image = 'python:3.13-alpine'
                        command = @('python', '-B', '-u', '-c', $StallCode)
                        ports = @(@{ name = 'http'; containerPort = 8080 })
                        readinessProbe = @{ tcpSocket = @{ port = 'http' }; periodSeconds = 1 }
                        securityContext = @{
                            allowPrivilegeEscalation = $false
                            readOnlyRootFilesystem = $true
                            capabilities = @{ drop = @('ALL') }
                        }
                    })
                }
            }
        }
    }
    kubectl --context push11b-traefik -n push wait --for=condition=Ready pod `
        -l "push-messenger/verification-fixture=$StallName" --timeout=60s
    NativeOK 'Ready controlled timeout backend'
    Wait-AuthEndpoints 8080
    Invoke-IngressFixture -LinksOnly -LinkStatus 504
} finally {
    try {
        kubectl --context push11b-traefik -n push delete job $StallName `
            --ignore-not-found --wait=true --timeout=120s | Out-Null
        NativeOK 'Remove only the owned timeout fixture'
    } finally {
        try {
            kubectl --context push11b-traefik -n push patch service push-auth --type=json `
                --patch '[{"op":"replace","path":"/spec/ports/0/targetPort","value":"http"}]' | Out-Null
            NativeOK 'Restore fixture Service port'
        } finally {
            kubectl --context push11b-traefik -n push scale deployment push-auth --replicas=$AuthReplicas
            NativeOK 'Restore fixture auth replicas'
            kubectl --context push11b-traefik -n push rollout status deployment/push-auth --timeout=180s
            NativeOK 'Restore healthy auth upstream'
        }
    }
}
Invoke-IngressFixture -LinksOnly -LinkStatus 200
```

After each state, retrieve logs for **all** controller replicas since `$Since`
into memory. Assert none contains `$Canary`, its encoded form or `?token=`.
Print only status counts and pass/fail, not matching lines. Check application
auth logs as well, and include controller startup/configuration errors in the
failure review. Ensure a known ordinary request produces an access entry:
empty/unavailable logs are a failed prerequisite, not token-safety proof.
Restore auth replicas, routes, selectors and target ports even after failure.

- [ ] **Step 6: Verify trusted forwarding and deployment-specific limits.**

On the same dedicated target, run these bounded checks after the account flow:

| Check | Required result |
|---|---|
| Send one login attempt with forged `X-Forwarded-For: 198.51.100.10` and `X-Real-IP: 198.51.100.11`; repeat with different forged values | Controller appends/overwrites according to the configured trust boundary; the application's last-entry parser must still select the actual peer, not either forged value |
| Use one unknown synthetic account and 11 sequential incorrect-password login attempts, alternating forged forwarding headers | First 10 are ordinary authentication rejections; 11th is 429 with Retry-After (per-account/IP limit in `push-auth\src\limits.rs`); changing client headers must not reset its identity |
| Send one syntactically valid but nonexistent invite redemption from each of the fixture's two devices, using different forged WebSocket handshake headers | Both receive protocol `INVITE_NOT_FOUND`; both increment the same real per-IP Redis quota, not separate forged-address quotas |
| Try direct backend traffic from an unrelated pod | Denied by actual enforcing-CNI policy, not merely absent from documentation |
| Reach the NodePort from an independently identified local client, not kubectl port-forward | Record what source address the controller actually sees, and whether minikube/NAT preserves it |

The Python fixture already performs the two gateway requests and the 11 login
attempts. Login has a 10-attempt per-account/IP budget; invite redemption has
20/user and 60/IP per hour. Exhausting the latter with only two users would hit
the user quota first and would **not** prove per-IP sharing. Instead, compare
the actual Redis counters before/after the fixture on this fresh installation:

```powershell
$keys = @(Get-FixtureRedeemKeys)
if ($keys.Count -ne 1) { throw 'Forged headers split the gateway IP quota' }
$count = kubectl --context push11b-traefik -n push exec $RedisPod -- sh -c `
    'export REDISCLI_AUTH="$REDIS_PASSWORD"; exec redis-cli get "$1"' sh $keys[0]
if ($LASTEXITCODE -ne 0 -or "$count".Trim() -ne '2') {
    throw 'Both real gateway requests must increment one IP quota'
}
foreach ($forged in '198.51.100.10', '198.51.100.11') {
    $hash = [Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($forged))
    $key = 'gw:redeem:ip:' + [Convert]::ToHexString($hash, 0, 16).ToLowerInvariant()
    if ($keys[0] -eq $key) { throw 'Gateway accepted a forged address' }
}
Write-Host 'ok   real gateway quota shared; neither forged address used'
```

The pre-guard is in Step 4, before the first full Python run; do not run the full
fixture twice to assemble the evidence. Record only equality/count outcomes,
not IP subjects or stored credentials. Never disable rate limits to pass these
checks or run them against a shared installation.
If the fixed-window boundary is crossed, repeat once with a new synthetic
account and report the inconclusive attempt, not an unqualified pass.

If a port-forward or minikube NAT collapses identities, record that limitation:
it can prove header sanitization at that hop but **not** external load-balancer
source-IP preservation. The consumer must separately accept their actual LB,
trusted hops and controller access/error/trace log sinks before production.
No production-readiness claim is inferred from a local tunnel.

- [ ] **Step 7: Exercise missing keys, source retention and image references live.**

On a separate disposable namespace/release, supply a consumer Secret missing
`resend-api-key` with sender `resend`, then one missing
`resend-webhook-secret` with webhook enabled. Require explicit Kubernetes
configuration failure and Helm wait failure; no successful deployment claim.
Restore the keys through the private provisioner and require recovery. Also
test absent base key `database-url`; do not let a generated fallback conceal it.

Run a second install/upgrade with the same external Secret and inspect that its
UID, data hash and ownership metadata are unchanged; do not compare by dumping
its values. Confirm disabling the webhook removes the environment reference
and public route without deleting the retained signing key.

Use chart tests as the exact digest-rendering proof. A live digest-pull check
requires actual consumer-published images: do not substitute Docker image/config
IDs for registry manifest digests or add a mandatory registry just for this plan.
If actual service images have been published by the operator, verify the runtime
image IDs against those pins; otherwise report that optional deployment-specific
check as not performed, without weakening the required syntax/render tests.

- [ ] **Step 8: Record acceptance and leave owned profiles stopped, data retained.**

The guide's results table must include source revision, Helm/Kubernetes/minikube/
Calico/controller chart/image versions, exact contexts, test case, expected
result, observed result and any limitation. Record outcomes only; no tokens,
private keys, full Secret manifests or backup artifacts.

Stop only the two newly owned profiles:

```powershell
minikube stop -p push11b-local
if ($LASTEXITCODE -ne 0) { throw 'Could not stop owned local fixture' }
minikube stop -p push11b-traefik
if ($LASTEXITCODE -ne 0) { throw 'Could not stop owned Traefik fixture' }
```

Terminate only owned port-forward processes by their captured PIDs; delete only
uniquely named temporary probe resources and explicitly resolved private scratch
files according to the operator's retention policy. Do not delete PVCs/Secrets/
namespaces/profiles as an upgrade shortcut.

Commit the verification guide only after the recorded tests actually run:

```powershell
git add docs\deployment-ingress-verification.md
git commit -m "docs: verify portable ingress on maintained Traefik" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 8: Final acceptance and review handoff

- [ ] **Step 1: Run the smallest final static suites covering these changes.**

```powershell
pwsh .\deploy\helm\test-chart.ps1
pwsh .\deploy\minikube\test-upgrade-preflight.ps1
```

Run the shell preflight suite in the repository's existing POSIX environment as
well; it uses command mocks, not a live upgrade. No Rust application change is
planned. If implementation needs Rust changes, stop and justify the scope, add
focused tests and run affected cargo tests, formatting, Clippy and MSRV checks;
do not silently broaden this chart plan into a service refactor.

- [ ] **Step 2: Check each required outcome against its actual proof.**

| Requirement | Proof |
|---|---|
| External Secret is references-only, no conflicts or broad injection | Tasks 1-3 structural key matrix, conflict failures and Task 7 ownership retention |
| Offline external render, including Resend/webhooks | Tasks 1-3 deterministic no-kubeconfig renders plus managed-only lookup control-flow review |
| Malformed managed auth keys rejected | Task 1 format/length/canonical-base64 failures; existing generated local keys still work |
| Explicit missing-key failure | Task 7 actual CreateContainerConfigError/Helm wait failures and repair |
| Configurable standard Ingress, annotation precedence, Exact link routes | Task 4 structural resource/path assertions |
| TLS on all HTTPS routes, valid SNI/trust, no disabled validation | Task 4 structural TLS checks and Task 7 live positive/negative TLS tests |
| WebSocket upgrade, active heartbeat and intentional idle close | Task 7 timed authenticated routing checks |
| Error-path token safety | Task 7 both-link healthy/503/refused/timeout controller/application log checks |
| Trusted forwarding, no spoofed rate-limit identity | Task 7 live auth/gateway checks and enforcing-CNI direct-call denial |
| Consumer routes work without chart Ingress | Task 3 webhook policy and Task 4 no-Ingress public-identity assertions |
| Immutable image references for every application/hook | Task 5 all-image tests; full-string Postgres/Redis pins retained |
| Local workflow preserved | Task 7 fresh nginx-compatibility raw/SDK smoke and enforced network checks |
| No consumer infrastructure/pipeline added | Allowed-resource test, file/diff review and consumer-ownership docs |

Missing live prerequisites are blockers or explicitly bounded limitations, not
render-based substitutes. Mandatory Traefik routing acceptance must pass before
11B is called implemented; consumer LB/capacity acceptance is not part of 11B.

- [ ] **Step 3: Review the diff and update status accurately.**

```powershell
git diff --check
git --no-pager diff --stat main...HEAD
git status --short
```

Before merge, the roadmap says **11B implemented / under review**, not Done.
After merge, set Done with the actual PR reference. Keep 11A **Done (#20)**.
11C remains pending; do not claim a load harness, real email-domain deployment,
automatic secret rotation or 10,000-connection production capacity.

- [ ] **Step 4: Commit final related status/documentation changes and open the normal review.**

```powershell
git add docs\superpowers\plans\2026-10-11-roadmap.md docs\superpowers\specs\2026-10-16-portable-deployment-readiness-design.md
git commit -m "docs: record portable chart acceptance status" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Plan self-review and external references

Spec sections 5 and 6 map to Tasks 1-7; section 8's 11B acceptance maps to the
Task 8 table. Sections 1/2/9 constrain every task. Section 7's consumer build/load
tools stay in 11C except the image interface and minimal configuration examples.
The already merged sections 3/4 remain unchanged except their directly dependent
webhook-policy condition and documentation.

Verified reference sources when writing this plan:

- Traefik chart 42.0.0 release and pinned application version:
  <https://github.com/traefik/traefik-helm-chart/releases/tag/v42.0.0>,
  <https://raw.githubusercontent.com/traefik/traefik-helm-chart/v42.0.0/traefik/Chart.yaml>.
- Pinned verification-controller values and field names:
  <https://raw.githubusercontent.com/traefik/traefik-helm-chart/v42.0.0/traefik/values.yaml>.
- Standard Ingress/TLS routing:
  <https://doc.traefik.io/traefik/reference/routing-configuration/kubernetes/ingress/>.
- Forwarding trust/timeouts:
  <https://doc.traefik.io/traefik/reference/install-configuration/entrypoints/>.
- Access-log field/query filtering and error-log distinction:
  <https://doc.traefik.io/traefik/reference/install-configuration/observability/logs-and-accesslogs/>.

Do not copy controller settings into the controller-neutral application defaults.
Controller capabilities and supported versions must be rechecked at execution.
