# Plan 13B: Microsoft Entra ID Sign-In in the Chart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially on one branch.

**Goal:** Let consumers turn on Microsoft Entra ID sign-in (beside or instead of email and password) with chart values: auth-svc's sign-in variables, Entra-only installations without email (worker `none`, no Resend keys, no link-page route), egress for auth-svc to Entra ID or a proxy, a trusted CA bundle, the guide's chart section, and a minikube rehearsal of an Entra-only installation.

**Architecture:** One new values block, `auth.signIn`, validated by `push.checkSignIn` and rendered by `push.signInEnv` into auth-svc's environment. Two small helpers, `push.entraSignIn` and `push.emailOff`, steer the worker's sender, its DEV ONLY mailbox flag and the link-page Ingress. Plan 12B's opt-in egress policies gain an auth-only list `networkPolicy.egress.entraSignInCidrs`. The DEV ONLY `push-dev-entra` tool of 12B gains a `sign-in` mode serving 13A's stub metadata and keys over HTTPS and minting user tokens, and `push-smoke` gains an Entra mode. With every value at its default the rendered chart is byte-identical to before.

**Tech Stack:** Helm 3 templates, PowerShell + PyYAML chart checks (`deploy\helm\test-chart.ps1`), minikube (docker driver, Calico), the Plan 13A services and SDK, Plan 12B's `push-dev-entra` and `serve_tls`.

---

## Authority and boundaries

**Depends on Plan 12B being merged** (its egress values and `$egressOf` map in
`templates\networkpolicy.yaml`, `up.ps1 -SkipInstall`, the `push-dev-entra` binary, the
`stub` feature of push-credentials with `TestCa` and `serve_tls`) **and on Plan 13A** (the
variables, `PUSH_EMAIL_SENDER=none`, `push_auth::entra::testing`, `Client::sign_in_with_entra`).
Before starting, read the merged `templates\networkpolicy.yaml`, `templates\auth.yaml`,
`templates\_helpers.tpl` and `crates\push-smoke\src\bin\push-dev-entra.rs`: where this plan
names a 12B construct, use the merged name if it differs and note it in the PR.

Read first: `docs\superpowers\specs\2026-10-19-entra-sign-in-design.md` (D9, D14 and §3 are
binding), `docs\entra-sign-in.md`, `docs\deployment-configuration.md`.

The chart provisions nothing outside the release namespace and nothing in Entra ID. The
default render, the bundled datastores, `values-minikube.yaml` and the upgrade runbook stay
unchanged.

Start from clean, current `main`:

```powershell
git switch main
git pull --ff-only
git switch -c feature/plan-13b-entra-sign-in-chart
pwsh deploy/helm/test-chart.ps1
helm template push deploy/helm/push-messenger > $env:TEMP\push-default-before-13b.yaml
```

Expected: all chart checks pass. The saved render is the baseline for "unchanged".

Every commit ends with:

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

## Facts the design depends on

| Fact | Source | Consequence |
|---|---|---|
| auth-svc reads `PUSH_AUTH_PROVIDERS`, `PUSH_ENTRA_{CLIENT_ID,ALLOWED_TENANTS,AUTHORITY_HOST,REQUIRED_SCOPE,REQUIRED_ROLE,ALLOWED_CLIENT_APPS,MAX_SESSION_HOURS}` and only with `entra` the latter | Plan 13A Task 1 | The chart renders them only when `entra` is chosen |
| The worker accepts `PUSH_EMAIL_SENDER=none` and refuses it with `PUSH_RESEND_WEBHOOK_SECRET` or `PUSH_DEV_MAILBOX=1` | Plan 13A Task 11 | Entra-only renders `none` and mailbox `0`, and refuses the webhook |
| The link pages are a separate Ingress `push-auth-links` with Exact paths `/v1/auth/verify`, `/v1/auth/password/reset` | `templates\ingress.yaml` | Not rendered in Entra-only mode (auth-svc answers 404 there anyway) |
| The Resend keys are referenced only with `worker.email.sender: resend` or an enabled webhook | `templates\worker.yaml`, `templates\secret.yaml` | Entra-only needs no email key in a consumer-managed Secret |
| reqwest in auth-svc honours `HTTPS_PROXY`/`NO_PROXY`; rustls-native-certs 0.8.4 loads only `SSL_CERT_FILE` when it is set | reqwest 0.12.28, `rustls-native-certs-0.8.4\src\lib.rs:8,363` | `httpsProxy` affects only the Entra requests; a CA bundle must hold every root auth-svc needs |
| auth-svc accepts an `http://` authority only on loopback | Plan 13A Task 1 | The rehearsal's issuer serves HTTPS with 12B's test CA |
| 12B's egress map gives every application component `httpsCidrs` on 443 | Plan 12B Task 6 | A separate auth-only list keeps the other components closed |
| Entra ID's endpoints are `login.microsoftonline.com`; NetworkPolicies match IP blocks, not names | Microsoft Learn, Kubernetes | The guide offers the service tag ranges, an FQDN firewall, or a proxy |

## Values (the whole new surface)

```yaml
auth:
  signIn:
    # password (default), entra, or password,entra (docs/entra-sign-in.md). With entra
    # alone, the worker sends no email (sender none) and the link pages are not routed.
    providers: password
    entra:
      # The API registration's application id; optionally also its api:// identifier URI.
      clientIds: []
      # Tenant ids (GUIDs); common, organizations and consumers are not supported.
      tenants: []
      authorityHost: https://login.microsoftonline.com/
      # The delegated scope tokens must carry; * for any scope of the API.
      requiredScope: access_as_user
      requiredRole: ""
      # Client app ids allowed to present tokens (azp); empty allows any.
      allowedClientApps: []
      # An Entra-backed device signs in with Entra ID again after this many hours (1-720).
      maxSessionHours: 24
      # A proxy for auth-svc's requests to Entra ID (HTTPS_PROXY); NO_PROXY keeps
      # in-cluster names direct.
      httpsProxy: ""
      # A ConfigMap in the release namespace whose ca.crt auth-svc trusts instead of the
      # system roots (SSL_CERT_FILE), e.g. a TLS-inspecting proxy's root plus the public
      # roots it needs. Empty: the system roots.
      caConfigMap: ""
worker:
  email:
    # log, resend, or none (no email at all; rendered automatically with entra alone).
    sender: log
networkPolicy:
  egress:
    # With egress enabled (Plan 12B): CIDRs auth-svc alone may reach on 443 for Entra ID
    # sign-in (Microsoft's AzureActiveDirectory service tag ranges).
    entraSignInCidrs: []
smoke:
  # DEV ONLY: the rehearsal's token endpoint; the smoke test then runs in Entra mode.
  entraTokenUrl: ""
```

## Files

| File | Change |
|---|---|
| `deploy\helm\push-messenger\values.yaml` | the values above |
| `templates\_helpers.tpl` | `push.entraSignIn`, `push.emailOff`, `push.mailboxFlag`, `push.checkSignIn`, `push.signInEnv`; `push.checkEmail` accepts `none` |
| `templates\validate.yaml` | calls `push.checkSignIn` |
| `templates\auth.yaml` | sign-in environment, CA bundle volume |
| `templates\worker.yaml` | sender and mailbox flag through the helpers |
| `templates\ingress.yaml` | no `auth-links` route without email |
| `templates\networkpolicy.yaml` | `entraSignInCidrs` for auth-svc |
| `templates\tests\smoke.yaml` | `--entra-token-url` |
| `deploy\helm\test-chart.ps1` | the checks |
| `crates\push-smoke\Cargo.toml`, `src\bin\push-dev-entra.rs`, `src\entra.rs`, `src\lib.rs`, `src\main.rs` | DEV ONLY sign-in issuer; the Entra smoke mode |
| `deploy\minikube\entra\sign-in-issuer.yaml`, `deploy\minikube\values-entra-sign-in-rehearsal.yaml`, `deploy\minikube\entra-sign-in-rehearsal.ps1` | the rehearsal |
| `docs\entra-sign-in.md`, `docs\deployment-configuration.md`, `README.md`, roadmap, spec | chart section, links, status |

---

### Task 1: Values and validation

**Files:**
- Modify: `deploy\helm\push-messenger\values.yaml`, `templates\_helpers.tpl`, `templates\validate.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Write the failing checks.** Append to `test-chart.ps1` before its final
summary:

```powershell
# ---- Entra ID sign-in (Plan 13B) ----
$signInArgs = @(
    '--set', 'auth.signIn.providers=entra',
    '--set', 'auth.signIn.entra.clientIds={11111111-0000-4000-8000-000000000001}',
    '--set', 'auth.signIn.entra.tenants={33333333-0000-4000-8000-000000000003}'
)
Check 'sign-in: a complete Entra-only configuration renders' { (Render $signInArgs) -eq 0 }
Check 'sign-in: both providers render' {
    (Render ($signInArgs + @('--set', 'auth.signIn.providers=password\,entra'))) -eq 0
}
Check 'sign-in: unknown providers and incomplete Entra settings are refused' {
    (Render @('--set', 'auth.signIn.providers=ldap')) -ne 0 -and
        (Render @('--set', 'auth.signIn.providers=entra')) -ne 0 -and
        (Render ($signInArgs + @('--set-json', 'auth.signIn.entra.tenants=[]'))) -ne 0 -and
        (Render ($signInArgs + @('--set-json', 'auth.signIn.entra.clientIds=[]'))) -ne 0
}
Check 'sign-in: wildcard tenants and personal accounts are refused' {
    (Render ($signInArgs + @('--set', 'auth.signIn.entra.tenants={common}'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.tenants={organizations}'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.tenants={9188040d-6c67-4c5b-b112-36a304b66dad}'))) -ne 0
}
Check 'sign-in: the audience must include the application id' {
    (Render ($signInArgs + @('--set', 'auth.signIn.entra.clientIds={api://push-api}'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.clientIds={api://push-api,11111111-0000-4000-8000-000000000001}'))) -eq 0
}
Check 'sign-in: authority, scope, session age and proxy are checked' {
    (Render ($signInArgs + @('--set', 'auth.signIn.entra.authorityHost=http://login.microsoftonline.com/'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.requiredScope=two scopes'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.maxSessionHours=0'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.maxSessionHours=721'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.httpsProxy=proxy:3128'))) -ne 0
}
Check 'sign-in: Entra-only refuses the Resend sender and the webhook' {
    (Render ($signInArgs + @('--set', 'worker.email.sender=resend', '--set', 'worker.email.resendApiKey=re_x',
        '--set', 'worker.email.from=a@example.com'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'worker.email.webhook.enabled=true',
            '--set', 'worker.email.webhookSecret=whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw'))) -ne 0
}
Check 'email: the none sender is accepted and refuses the webhook' {
    (Render @('--set', 'worker.email.sender=none')) -eq 0 -and
        (Render @('--set', 'worker.email.sender=none', '--set', 'worker.email.webhook.enabled=true',
            '--set', 'worker.email.webhookSecret=whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw')) -ne 0
}
Check 'default: the render is unchanged by Plan 13B' {
    $before = Get-Content (Join-Path $env:TEMP 'push-default-before-13b.yaml') -Raw -ErrorAction SilentlyContinue
    $now = helm template push $chart | Out-String
    (-not $before) -or (($before -replace "`r`n", "`n") -eq ($now -replace "`r`n", "`n"))
}
```

(The comma in `password\,entra` is escaped for `--set`. The "unchanged" check passes
trivially on CI, where the baseline file is absent; the other default checks guard CI.)

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: the refusal checks FAIL (the values do not exist; Helm renders anything).

- [ ] **Step 3: Values.** Add the `auth.signIn` block to `values.yaml` inside the existing
`auth:` map (after `linkKey`), change the `worker.email.sender` comment to the one above, add
`entraSignInCidrs: []` with its comment inside 12B's `networkPolicy.egress` map, and add
`entraTokenUrl: ""` with its comment at the end of `smoke:`.

- [ ] **Step 4: Helpers.** Add to `_helpers.tpl`:

```yaml
{{/* "true" when Microsoft Entra ID sign-in is offered (spec 2026-10-19). */}}
{{- define "push.entraSignIn" -}}
{{- if has "entra" (splitList "," (nospace .Values.auth.signIn.providers)) -}}true{{- end -}}
{{- end -}}

{{/* "true" when email and password sign-in is off: nothing ever writes an email. */}}
{{- define "push.emailOff" -}}
{{- if not (has "password" (splitList "," (nospace .Values.auth.signIn.providers))) -}}true{{- end -}}
{{- end -}}

{{/* "1" or "0" for PUSH_DEV_MAILBOX: dev mode, and only while the worker logs email. */}}
{{- define "push.mailboxFlag" -}}
{{- if or (include "push.emailOff" .) (eq .Values.worker.email.sender "none") -}}0{{- else -}}{{ include "push.devFlag" . }}{{- end -}}
{{- end -}}

{{/*
Checks the sign-in settings (spec 2026-10-19 §3): the providers; with entra the API's
application id (optionally its api:// URI), tenant ids (never common, organizations,
consumers or the personal-account tenant), an https authority host, one scope name or *,
1 to 720 session hours, an http(s) proxy URL. Entra-only installations write no email, so
the Resend sender and the webhook are refused with them.
*/}}
{{- define "push.checkSignIn" -}}
{{- $signIn := .Values.auth.signIn -}}
{{- $providers := nospace (toString $signIn.providers) -}}
{{- if not (has $providers (list "password" "entra" "password,entra" "entra,password")) -}}
{{- fail (printf "auth.signIn.providers must be password, entra or password,entra, not %q" $signIn.providers) -}}
{{- end -}}
{{- if include "push.entraSignIn" . -}}
{{- $entra := $signIn.entra -}}
{{- $guid := "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$" -}}
{{- if not $entra.clientIds -}}
{{- fail "auth.signIn.entra.clientIds needs the API registration's application id" -}}
{{- end -}}
{{- $hasId := false -}}
{{- range $entra.clientIds -}}
{{- if regexMatch $guid (toString .) -}}
{{- $hasId = true -}}
{{- else if not (regexMatch "^api://[!-~]+$" (toString .)) -}}
{{- fail (printf "auth.signIn.entra.clientIds: %q is neither an application id nor an api:// URI" .) -}}
{{- end -}}
{{- end -}}
{{- if not $hasId -}}
{{- fail "auth.signIn.entra.clientIds must include the application id: v2.0 access tokens carry it as their audience" -}}
{{- end -}}
{{- if not $entra.tenants -}}
{{- fail "auth.signIn.entra.tenants needs your tenant id" -}}
{{- end -}}
{{- range $entra.tenants -}}
{{- if or (not (regexMatch $guid (toString .))) (eq (lower (toString .)) "9188040d-6c67-4c5b-b112-36a304b66dad") -}}
{{- fail (printf "auth.signIn.entra.tenants: %q is not a workforce tenant id (common, organizations and consumers are not supported)" .) -}}
{{- end -}}
{{- end -}}
{{- range $entra.allowedClientApps -}}
{{- if not (regexMatch $guid (toString .)) -}}
{{- fail (printf "auth.signIn.entra.allowedClientApps: %q is not an application id" .) -}}
{{- end -}}
{{- end -}}
{{- if not (regexMatch "^https://[^/@?#]+/?$" (toString $entra.authorityHost)) -}}
{{- fail "auth.signIn.entra.authorityHost must be https://host/" -}}
{{- end -}}
{{- if not (regexMatch "^[!-~]{1,128}$" (toString $entra.requiredScope)) -}}
{{- fail "auth.signIn.entra.requiredScope must be one scope name or *" -}}
{{- end -}}
{{- if and $entra.requiredRole (not (regexMatch "^[!-~]{1,128}$" (toString $entra.requiredRole))) -}}
{{- fail "auth.signIn.entra.requiredRole must be one app role value" -}}
{{- end -}}
{{- $hours := int $entra.maxSessionHours -}}
{{- if or (lt $hours 1) (gt $hours 720) -}}
{{- fail "auth.signIn.entra.maxSessionHours must be 1 to 720" -}}
{{- end -}}
{{- if and $entra.httpsProxy (not (regexMatch "^https?://[^ ]+$" (toString $entra.httpsProxy))) -}}
{{- fail "auth.signIn.entra.httpsProxy must be an http:// or https:// URL" -}}
{{- end -}}
{{- end -}}
{{- if include "push.emailOff" . -}}
{{- if eq .Values.worker.email.sender "resend" -}}
{{- fail "auth.signIn.providers entra writes no email: use worker.email.sender log or none (the chart renders none)" -}}
{{- end -}}
{{- if .Values.worker.email.webhook.enabled -}}
{{- fail "auth.signIn.providers entra writes no email: set worker.email.webhook.enabled false" -}}
{{- end -}}
{{- end -}}
{{- end -}}
```

In `push.checkEmail`, change the sender check to accept `none`:

```yaml
{{- if not (has $email.sender (list "log" "resend" "none")) -}}
{{- fail (printf "worker.email.sender must be log, resend or none, not %q" $email.sender) -}}
{{- end -}}
{{- if and (eq $email.sender "none") $email.webhook.enabled -}}
{{- fail "worker.email.sender none sends no email: set worker.email.webhook.enabled false" -}}
{{- end -}}
```

and in `templates\validate.yaml` add `{{- include "push.checkSignIn" . -}}` after
`push.checkEmail`.

- [ ] **Step 5: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: every check passes, the new ones and the default-unchanged check included (nothing
renders the values yet).

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(chart): add the sign-in values and their validation"
```

### Task 2: auth-svc's sign-in environment, proxy and CA bundle

**Files:**
- Modify: `templates\_helpers.tpl`, `templates\auth.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Write the failing checks.**

```powershell
Check 'sign-in: the default renders no sign-in variable' {
    (helm template push $chart | Out-String) -notmatch 'PUSH_AUTH_PROVIDERS|PUSH_ENTRA_|HTTPS_PROXY|entra-ca'
}
Check 'sign-in: Entra-only renders auth-svc''s variables' {
    $auth = Doc (helm template push $chart @signInArgs | Out-String) 'Deployment' 'push-auth'
    $auth -match '(?s)name: PUSH_AUTH_PROVIDERS\s+value: "entra"' -and
        $auth -match '(?s)name: PUSH_ENTRA_CLIENT_ID\s+value: "11111111-0000-4000-8000-000000000001"' -and
        $auth -match '(?s)name: PUSH_ENTRA_ALLOWED_TENANTS\s+value: "33333333-0000-4000-8000-000000000003"' -and
        $auth -match '(?s)name: PUSH_ENTRA_AUTHORITY_HOST\s+value: "https://login.microsoftonline.com/"' -and
        $auth -match '(?s)name: PUSH_ENTRA_REQUIRED_SCOPE\s+value: "access_as_user"' -and
        $auth -match '(?s)name: PUSH_ENTRA_MAX_SESSION_HOURS\s+value: "24"' -and
        $auth -notmatch 'PUSH_ENTRA_REQUIRED_ROLE|PUSH_ENTRA_ALLOWED_CLIENT_APPS|HTTPS_PROXY'
}
Check 'sign-in: optional settings, a proxy and a CA bundle reach auth-svc only' {
    $rendered = helm template push $chart @signInArgs `
        --set auth.signIn.entra.requiredRole=Messenger.User `
        --set 'auth.signIn.entra.allowedClientApps={22222222-0000-4000-8000-000000000002}' `
        --set auth.signIn.entra.httpsProxy=http://proxy.corp.example:3128 `
        --set auth.signIn.entra.caConfigMap=corp-ca | Out-String
    $auth = Doc $rendered 'Deployment' 'push-auth'
    $auth -match '(?s)name: PUSH_ENTRA_REQUIRED_ROLE\s+value: "Messenger.User"' -and
        $auth -match '(?s)name: PUSH_ENTRA_ALLOWED_CLIENT_APPS\s+value: "22222222-0000-4000-8000-000000000002"' -and
        $auth -match '(?s)name: HTTPS_PROXY\s+value: "http://proxy.corp.example:3128"' -and
        $auth -match '(?s)name: NO_PROXY\s+value: "localhost,127.0.0.1,.svc,.cluster.local"' -and
        $auth -match '(?s)name: SSL_CERT_FILE\s+value: "/etc/push/entra-ca/ca.crt"' -and
        $auth -match '(?s)configMap:\s+name: corp-ca' -and $auth -match 'mountPath: /etc/push/entra-ca' -and
        (Doc $rendered 'Deployment' 'push-gateway') -notmatch 'HTTPS_PROXY|entra-ca'
}
Check 'sign-in: password,entra keeps the email routes and offers both' {
    $auth = Doc (helm template push $chart @signInArgs --set 'auth.signIn.providers=password\,entra' | Out-String) 'Deployment' 'push-auth'
    $auth -match '(?s)name: PUSH_AUTH_PROVIDERS\s+value: "password,entra"'
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: the three rendering checks FAIL.

- [ ] **Step 3: The helper.** Add to `_helpers.tpl`:

```yaml
{{/*
auth-svc's sign-in variables (spec 2026-10-19 §3), as env list entries; nothing for the
default (password only). The proxy serves only auth-svc's requests to Entra ID (reqwest):
gRPC and the datastores never read it. A CA bundle replaces the system roots for every TLS
connection auth-svc makes (rustls-native-certs reads SSL_CERT_FILE alone).
*/}}
{{- define "push.signInEnv" -}}
{{- $signIn := .Values.auth.signIn -}}
{{- $providers := nospace (toString $signIn.providers) -}}
{{- if ne $providers "password" }}
- name: PUSH_AUTH_PROVIDERS
  value: {{ $providers | quote }}
{{- end }}
{{- if include "push.entraSignIn" . }}
{{- $entra := $signIn.entra }}
- name: PUSH_ENTRA_CLIENT_ID
  value: {{ join "," $entra.clientIds | quote }}
- name: PUSH_ENTRA_ALLOWED_TENANTS
  value: {{ join "," $entra.tenants | quote }}
- name: PUSH_ENTRA_AUTHORITY_HOST
  value: {{ $entra.authorityHost | quote }}
- name: PUSH_ENTRA_REQUIRED_SCOPE
  value: {{ $entra.requiredScope | quote }}
{{- with $entra.requiredRole }}
- name: PUSH_ENTRA_REQUIRED_ROLE
  value: {{ . | quote }}
{{- end }}
{{- with $entra.allowedClientApps }}
- name: PUSH_ENTRA_ALLOWED_CLIENT_APPS
  value: {{ join "," . | quote }}
{{- end }}
- name: PUSH_ENTRA_MAX_SESSION_HOURS
  value: {{ $entra.maxSessionHours | quote }}
{{- with $entra.httpsProxy }}
- name: HTTPS_PROXY
  value: {{ . | quote }}
- name: NO_PROXY
  value: "localhost,127.0.0.1,.svc,.cluster.local"
{{- end }}
{{- if $entra.caConfigMap }}
- name: SSL_CERT_FILE
  value: "/etc/push/entra-ca/ca.crt"
{{- end }}
{{- end }}
{{- end -}}

{{/* "true" when auth-svc mounts the sign-in CA bundle. */}}
{{- define "push.signInCa" -}}
{{- if and (include "push.entraSignIn" .) .Values.auth.signIn.entra.caConfigMap -}}true{{- end -}}
{{- end -}}
```

- [ ] **Step 4: Render it.** In `templates\auth.yaml`, after the `PUSH_DIRECTORY_TOKEN` env
entry (the last one), add:

```yaml
            {{- with (include "push.signInEnv" . | trim) }}
            {{- . | nindent 12 }}
            {{- end }}
```

(`with` on the trimmed text renders nothing at all for the default, so the default render
stays byte-identical.)

Then the CA bundle. Plan 12B renders auth-svc's datastore CA bundles as a container
`volumeMounts:` list and a pod `volumes:` list under its own conditions. Add the sign-in
bundle to those same lists, and make each list's guard also true when
`include "push.signInCa" .` is: the container entry

```yaml
            {{- if include "push.signInCa" . }}
            - name: entra-ca
              mountPath: /etc/push/entra-ca
              readOnly: true
            {{- end }}
```

and the pod entry

```yaml
        {{- if include "push.signInCa" . }}
        - name: entra-ca
          configMap:
            name: {{ .Values.auth.signIn.entra.caConfigMap }}
            items:
              - key: ca.crt
                path: ca.crt
        {{- end }}
```

If the merged `auth.yaml` has no such lists, add `volumeMounts:` (after `ports:`) and
`volumes:` (after `containers:`) wrapped in `{{- if include "push.signInCa" . }}` with the
entries above. Plan 12B's chart-injection mode also sets `SSL_CERT_FILE` (from
`azure.workloadIdentity.authorityCaConfigMap`); add to `push.checkSignIn`:

```yaml
{{- if and (include "push.signInCa" .) .Values.azure.workloadIdentity.authorityCaConfigMap (ne .Values.azure.workloadIdentity.authorityCaConfigMap .Values.auth.signIn.entra.caConfigMap) -}}
{{- fail "auth-svc trusts one CA bundle: set auth.signIn.entra.caConfigMap to the same ConfigMap as azure.workloadIdentity.authorityCaConfigMap, holding every root it needs" -}}
{{- end -}}
```

and render auth-svc's `SSL_CERT_FILE` and mount once when both name the same ConfigMap (skip
the sign-in entries then; 12B's are the same file).

- [ ] **Step 5: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: every check passes; `helm lint deploy/helm/push-messenger` with the sign-in
arguments passes.

- [ ] **Step 6: Commit.**

```powershell
git add -A
git commit -m "feat(chart): render auth-svc's Entra ID sign-in settings, proxy and CA bundle"
```

### Task 3: Entra-only installations write no email

**Files:**
- Modify: `templates\worker.yaml`, `templates\ingress.yaml`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Write the failing checks.**

```powershell
Check 'sign-in: Entra-only sends no email and routes no link page' {
    $rendered = helm template push $chart @signInArgs | Out-String
    $worker = Doc $rendered 'Deployment' 'push-worker'
    $worker -match '(?s)name: PUSH_EMAIL_SENDER\s+value: "none"' -and
        $worker -match '(?s)name: PUSH_DEV_MAILBOX\s+value: "0"' -and
        $worker -notmatch 'resend-api-key|resend-webhook-secret' -and
        -not (Doc $rendered 'Ingress' 'push-auth-links') -and
        (Doc $rendered 'Ingress' 'push-auth') -match 'path: /v1/auth'
}
Check 'sign-in: Entra-only in dev mode keeps the mailbox off' {
    $rendered = helm template push $chart -f $minikube @signInArgs --set worker.email.webhook.enabled=false | Out-String
    (Doc $rendered 'Deployment' 'push-worker') -match '(?s)name: PUSH_DEV_MAILBOX\s+value: "0"'
}
Check 'sign-in: password,entra keeps the link pages and the sender' {
    $rendered = helm template push $chart @signInArgs --set 'auth.signIn.providers=password\,entra' | Out-String
    (Doc $rendered 'Ingress' 'push-auth-links') -and
        (Doc $rendered 'Deployment' 'push-worker') -match '(?s)name: PUSH_EMAIL_SENDER\s+value: "log"'
}
Check 'sign-in: an Entra-only consumer Secret needs no email key' {
    $rendered = helm template push $chart @signInArgs --set secrets.existingSecret=push-creds | Out-String
    $rendered -notmatch 'key: resend-'
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: the first three FAIL (the worker logs email and the link Ingress renders).

- [ ] **Step 3: Implement.** In `templates\worker.yaml` change the sender and mailbox
entries to:

```yaml
            # none when email and password sign-in is off: nothing writes an email then.
            - name: PUSH_EMAIL_SENDER
              value: {{ ternary "none" .Values.worker.email.sender (eq (include "push.emailOff" .) "true") | quote }}
```

```yaml
            - name: PUSH_DEV_MAILBOX
              value: {{ include "push.mailboxFlag" . | quote }}
```

(the `resend-api-key` reference stays under `{{- if eq .Values.worker.email.sender "resend" }}`,
which validation refuses together with Entra-only).

In `templates\ingress.yaml`, build the `auth-links` route only with email:

```yaml
{{- $routes := list
  (dict "name" "gateway" "component" "gateway" "paths" (list "/v1/socket") "type" "Exact" "annotations" .Values.ingress.gatewayAnnotations)
  (dict "name" "auth" "component" "auth" "paths" (list "/v1/auth" "/v1/devices" "/v1/users") "type" "Prefix" "annotations" .Values.ingress.authAnnotations)
-}}
{{- if not (include "push.emailOff" .) -}}
{{- $routes = append $routes (dict "name" "auth-links" "component" "auth" "paths" (list "/v1/auth/verify" "/v1/auth/password/reset") "type" "Exact" "annotations" .Values.ingress.authLinkAnnotations) -}}
{{- end -}}
```

(the order of the rendered Ingresses is unchanged for the default) and add to the template's
leading comment: "Without email and password sign-in there are no link pages, so no
`auth-links` Ingress."

- [ ] **Step 4: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: every check passes, the default-unchanged one included.

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(chart): send no email and route no link page in Entra-only installations"
```

### Task 4: Egress to Microsoft Entra ID for auth-svc only

**Files:**
- Modify: `templates\networkpolicy.yaml`, `templates\_helpers.tpl`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Write the failing checks.**

```powershell
Check 'sign-in egress: auth-svc alone reaches the Entra ID ranges on 443' {
    $rendered = helm template push $chart @signInArgs --set networkPolicy.egress.enabled=true `
        --set 'networkPolicy.egress.entraSignInCidrs={20.190.128.0/18}' | Out-String
    $policies = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
policies = {d["metadata"]["name"]: d["spec"] for d in docs if d["kind"] == "NetworkPolicy"}
def reaches(name):
    return [(p["ipBlock"]["cidr"], port["port"]) for rule in policies[name].get("egress", [])
            for p in rule.get("to", []) if "ipBlock" in p for port in rule.get("ports", [])]
assert ("20.190.128.0/18", 443) in reaches("push-auth"), reaches("push-auth")
for other in ["push-gateway", "push-message-svc", "push-worker"]:
    assert ("20.190.128.0/18", 443) not in reaches(other), other
'@
    $rendered | & $python.Source -c $policies; $LASTEXITCODE -eq 0
}
Check 'sign-in egress: Entra sign-in with egress needs a way to Entra ID' {
    (Render ($signInArgs + @('--set', 'networkPolicy.egress.enabled=true'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'networkPolicy.egress.enabled=true',
            '--set', 'auth.signIn.entra.httpsProxy=http://proxy.corp.example:3128'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'networkPolicy.egress.enabled=true',
            '--set', 'networkPolicy.egress.httpsCidrs={20.190.128.0/18}'))) -eq 0
}
Check 'sign-in egress: without sign-in the list adds nothing' {
    $rendered = helm template push $chart --set networkPolicy.egress.enabled=true `
        --set 'networkPolicy.egress.entraSignInCidrs={20.190.128.0/18}' | Out-String
    $rendered -notmatch '20.190.128.0/18'
}
```

- [ ] **Step 2: Run them to see them fail.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: the first two FAIL.

- [ ] **Step 3: Implement.** In `templates\networkpolicy.yaml`, before 12B's `$egressOf`
map, add:

```yaml
{{- $signIn := list -}}
{{- if include "push.entraSignIn" . -}}
{{- range $egress.entraSignInCidrs }}{{- $signIn = append $signIn (dict "cidr" . "port" 443) -}}{{- end -}}
{{- end -}}
```

and append `$signIn` to auth-svc's entry of the map (12B's
`"auth" (concat (list (dict "peer" "message-svc" "port" 50053)) $postgresTo $redisTo $https)`
becomes `… $https $signIn)`). In `push.checkSignIn` add, inside the `push.entraSignIn`
branch:

```yaml
{{- $egress := .Values.networkPolicy.egress -}}
{{- range $egress.entraSignInCidrs -}}
{{- if not (regexMatch "^[0-9a-fA-F:.]+/[0-9]{1,3}$" (toString .)) -}}
{{- fail (printf "networkPolicy.egress.entraSignInCidrs: %q is not a CIDR" .) -}}
{{- end -}}
{{- end -}}
{{- if and $egress.enabled (not (or $egress.entraSignInCidrs $egress.httpsCidrs $egress.extra $entra.httpsProxy)) -}}
{{- fail "Entra ID sign-in with networkPolicy.egress.enabled: auth-svc must reach login.microsoftonline.com. Set networkPolicy.egress.entraSignInCidrs (the AzureActiveDirectory service tag ranges), httpsCidrs or extra rules, or auth.signIn.entra.httpsProxy with a rule to the proxy in extra" -}}
{{- end -}}
```

- [ ] **Step 4: Run.**

Run: `pwsh deploy/helm/test-chart.ps1`
Expected: every check passes, 12B's egress checks included.

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "feat(chart): let auth-svc alone reach Microsoft Entra ID under egress policies"
```

### Task 5: The guide's chart section and the configuration contract

**Files:**
- Modify: `docs\entra-sign-in.md`, `docs\deployment-configuration.md`, `README.md`

- [ ] **Step 1: Guide.** In `docs\entra-sign-in.md`, after "3. Configure auth-svc", add:

````markdown
### With the Helm chart

```yaml
auth:
  signIn:
    providers: entra            # or "password,entra"
    entra:
      clientIds:
        - 11111111-0000-4000-8000-000000000001   # the API's application id
      tenants:
        - 33333333-0000-4000-8000-000000000003   # your tenant id
      maxSessionHours: 24
      # requiredRole: Messenger.User
      # allowedClientApps: [22222222-0000-4000-8000-000000000002]
      # httpsProxy: http://proxy.corp.example:3128
      # caConfigMap: corp-ca      # ca.crt: every root auth-svc must trust
networkPolicy:
  egress:
    enabled: true                 # Plan 12B
    entraSignInCidrs: []          # AzureActiveDirectory service tag ranges, or use a proxy
```

With `providers: entra` the chart renders `PUSH_EMAIL_SENDER=none` for the worker, no link
page Ingress, and references no Resend key: a consumer-managed Secret needs no email keys.
`worker.email.sender: resend` and `worker.email.webhook.enabled: true` are refused then.
The `/v1/auth` Ingress route already carries `/v1/auth/entra`.
````

- [ ] **Step 2: Configuration contract.** In `docs\deployment-configuration.md`, after the
Entra ID datastore section of 12B, add:

```markdown
### Microsoft Entra ID sign-in

`auth.signIn` turns on Entra ID sign-in beside or instead of email and password
([guide](entra-sign-in.md)). Entra-only (`providers: entra`) needs no `resend-api-key` or
`resend-webhook-secret` in a consumer-managed Secret and no email provider at all; every
other key is unchanged. auth-svc then needs HTTPS egress to `login.microsoftonline.com`
(`networkPolicy.egress.entraSignInCidrs` or a proxy).
```

and add `none` to the description of `worker.email.sender` where the email settings are
listed.

- [ ] **Step 3: README.** Replace "(chart values arrive with Plan 13B)" from Plan 13A with
"(chart: `auth.signIn`)".

- [ ] **Step 4: Commit.**

```powershell
git add -A
git commit -m "docs: document Entra ID sign-in in the chart"
```

### Task 6: A DEV ONLY sign-in issuer and the smoke test's Entra mode

The rehearsal needs Plan 13A's stub OpenID issuer in the cluster over HTTPS (auth-svc
refuses `http://` authorities off loopback) and a way for the smoke test to get user tokens.
Both are DEV ONLY and go into the smoke image only.

**Files:**
- Modify: `crates\push-smoke\Cargo.toml`, `crates\push-smoke\src\bin\push-dev-entra.rs`, `src\lib.rs`, `src\main.rs`, `templates\tests\smoke.yaml`
- Create: `crates\push-smoke\src\entra.rs`
- Test: `deploy\helm\test-chart.ps1`

- [ ] **Step 1: Dependencies.** In `crates\push-smoke\Cargo.toml` move push-auth from
`[dev-dependencies]` to `[dependencies]` as
`push-auth = { workspace = true, features = ["testing"] }` (the DEV ONLY tool mints tokens
with its public test key; push-smoke is never a service image), and add `axum.workspace = true`
to `[dependencies]` if Plan 12B did not.

- [ ] **Step 2: The issuer mode.** In `crates\push-smoke\src\bin\push-dev-entra.rs`, extend
the usage in the module doc with

```rust
//!   push-dev-entra sign-in                     an HTTPS stub of Entra ID sign-in (Plan 13B)
```

add the arm `Some("sign-in") => sign_in().await,` to `main`'s match (and `| push-dev-entra
sign-in` to its usage error), and add:

```rust
/// DEV ONLY: Microsoft Entra ID sign-in for the Plan 13B rehearsal. Serves Plan 13A's stub
/// OpenID metadata and key set over HTTPS on :8443 for auth-svc, and on :8080
/// `POST /dev/token {"oid", "tenant"?}`, which mints a user token with the public test key:
/// whoever reaches it signs in as anyone. Reads PUSH_DEV_ENTRA_TLS_DIR (server.crt,
/// server.key) and PUSH_DEV_SIGN_IN_AUTHORITY (`https://<service>.<namespace>.svc/`).
async fn sign_in() -> anyhow::Result<()> {
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use push_auth::entra::testing::{KEY_A, StubOpenId, TENANT, sign};
    use serde_json::{Value, json};

    let authority = var("PUSH_DEV_SIGN_IN_AUTHORITY");
    let (stub, metadata) = StubOpenId::with_authority(&authority);
    let mint = Router::new()
        .route(
            "/dev/token",
            post(
                |State(stub): State<Arc<StubOpenId>>, Json(body): Json<Value>| async move {
                    let oid = body["oid"].as_str().unwrap_or_default().to_owned();
                    if uuid::Uuid::try_parse(&oid).is_err() {
                        return Err(StatusCode::BAD_REQUEST);
                    }
                    let tenant = body["tenant"].as_str().unwrap_or(TENANT).to_owned();
                    let token = sign(KEY_A, &stub.user_claims(&tenant, &oid));
                    println!("minted a DEV ONLY Entra ID token for tenant {tenant}");
                    Ok(Json(json!({ "token": token })))
                },
            ),
        )
        .with_state(Arc::new(stub));
    let dir = std::path::PathBuf::from(var("PUSH_DEV_ENTRA_TLS_DIR"));
    let cert = std::fs::read_to_string(dir.join("server.crt"))?;
    let key = std::fs::read_to_string(dir.join("server.key"))?;
    let plain = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    tokio::spawn(async move { axum::serve(plain, mint).await });
    let tls = tokio::net::TcpListener::bind("0.0.0.0:8443").await?;
    println!("serving the DEV ONLY Entra ID sign-in stub: metadata on :8443, tokens on :8080");
    serve_tls(tls, metadata, &cert, &key).await;
    Ok(())
}
```

(add `uuid.workspace = true` and `serde_json` to push-smoke's `[dependencies]` if missing;
both are there today).

- [ ] **Step 3: The Entra smoke mode.** Create `crates\push-smoke\src\entra.rs`:

```rust
//! The smoke test of a Microsoft Entra ID-only deployment (Plan 13B), run instead of the
//! email/password one when `--entra-token-url` names the DEV ONLY issuer's token endpoint:
//! the email/password routes answer 404 through the ingress; two apps built on push-sdk
//! sign in with Entra ID tokens, register, become contacts by invite and chat; signing in
//! again keeps the device. Tokens are never printed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use push_sdk::{
    Client, Config, ConnectionState, ContactState, Event, Events, MemorySecretStore,
    MessageStatus, Platform, UserId,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{STEP_TIMEOUT, Target, WORKER_TIMEOUT, step};

struct App {
    name: &'static str,
    oid: String,
    client: Client,
    events: Events,
    user: UserId,
}

/// Runs the Entra ID smoke test against `target`, with user tokens from `token_url`.
pub async fn run(target: &Target, token_url: &str) -> anyhow::Result<()> {
    let started = Instant::now();
    let http = reqwest::Client::builder()
        .timeout(STEP_TIMEOUT)
        .no_proxy()
        .build()?;
    let mut login = http
        .post(format!("{}/v1/auth/login", target.auth_url.trim_end_matches('/')))
        .json(&json!({"email": "nobody@smoke.test", "password": "not a password"}));
    if let Some(host) = &target.host {
        login = login.header(reqwest::header::HOST, host);
    }
    let status = login.send().await.context("POST /v1/auth/login")?.status();
    if status != reqwest::StatusCode::NOT_FOUND {
        bail!("POST /v1/auth/login answered {status}, not 404: email and password sign-in is on");
    }
    step("email and password sign-in is off: POST /v1/auth/login answers 404");

    let mut erin = App::sign_in(target, &http, token_url, "erin").await?;
    let mut finn = App::sign_in(target, &http, token_url, "finn").await?;
    let invite = erin.client.create_invite(600, 1).await?;
    if finn.client.redeem_invite(&invite.code).await? != erin.user {
        bail!("finn's redeem named someone other than erin");
    }
    let finn_user = finn.user;
    erin.wait("the new contact", STEP_TIMEOUT, |event| {
        matches!(event, Event::ContactChanged(contact)
            if contact.user_id == finn_user && contact.state == ContactState::Accepted)
    })
    .await?;
    let chat = erin.client.open_direct(finn.user).await?.id;
    let text = format!("hello over Entra ID {}", Uuid::now_v7());
    let id = erin.client.send(chat, &text)?.client_msg_id;
    let Event::MessageAdded(received) = finn
        .wait("the message", STEP_TIMEOUT, |event| {
            matches!(event, Event::MessageAdded(message) if message.client_msg_id == id)
        })
        .await?
    else {
        unreachable!("the wait accepts only MessageAdded");
    };
    if received.body.as_deref() != Some(text.as_str()) || received.status != MessageStatus::Received
    {
        bail!("finn's app received the wrong message");
    }
    step("erin and finn signed in with Entra ID, became contacts by invite and chatted");

    let device = erin
        .client
        .account()?
        .context("erin is signed in")?
        .device_id;
    let token = mint(&http, token_url, &erin.oid).await?;
    let again = erin
        .client
        .sign_in_with_entra(&token, None, "erin's app", Platform::Other)
        .await?;
    if again.device_id != device {
        bail!("signing in again registered another device");
    }
    step("erin signed in with Entra ID again: the same device");
    for app in [erin, finn] {
        app.client.close().await?;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

/// A user token for `oid` from the DEV ONLY issuer.
async fn mint(http: &reqwest::Client, token_url: &str, oid: &str) -> anyhow::Result<String> {
    let answer: Value = http
        .post(token_url)
        .json(&json!({ "oid": oid }))
        .send()
        .await
        .context("the DEV ONLY token endpoint")?
        .error_for_status()?
        .json()
        .await?;
    answer["token"]
        .as_str()
        .map(str::to_owned)
        .context("the token endpoint answered no token")
}

impl App {
    /// A new user `name` signs in with Entra ID (choosing a handle), registers and connects.
    async fn sign_in(
        target: &Target,
        http: &reqwest::Client,
        token_url: &str,
        name: &'static str,
    ) -> anyhow::Result<Self> {
        let oid = Uuid::now_v7().to_string();
        let suffix = &oid[oid.len() - 8..];
        let database = format!(
            "{}/{name}-entra-{suffix}.db",
            target.data_dir.trim_end_matches('/')
        );
        let mut config = Config::new(&target.auth_url, &target.socket_url, &database);
        config.host = target.host.clone();
        // The smoke test reaches the cluster's Services over plain HTTP, inside the cluster.
        config.allow_insecure_transport = true;
        let (client, events) = Client::open(config, Arc::new(MemorySecretStore::new()))?;
        let token = mint(http, token_url, &oid).await?;
        let account = client
            .sign_in_with_entra(
                &token,
                Some(&format!("{name}_{suffix}")),
                &format!("{name}'s app"),
                Platform::Other,
            )
            .await
            .context("sign in with Entra ID")?;
        client.connect()?;
        let mut app = Self {
            name,
            oid,
            client,
            events,
            user: account.user_id,
        };
        app.wait("Connected", WORKER_TIMEOUT, |event| {
            matches!(
                event,
                Event::ConnectionStateChanged(ConnectionState::Connected)
            )
        })
        .await?;
        step(&format!(
            "{name} signed in with Entra ID and connected (device {})",
            account.device_id
        ));
        Ok(app)
    }

    async fn wait(
        &mut self,
        what: &str,
        timeout: Duration,
        wanted: impl Fn(&Event) -> bool,
    ) -> anyhow::Result<Event> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = tokio::time::timeout_at(deadline, self.events.next())
                .await
                .map_err(|_| anyhow::anyhow!("{}: no {what} in time", self.name))?
                .with_context(|| format!("{}: the client is gone", self.name))?;
            if wanted(&event) {
                return Ok(event);
            }
        }
    }
}
```

In `src\lib.rs` add `pub mod entra;` after `mod auth;`.

Replace `src\main.rs`'s `main` with:

```rust
#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // DEV ONLY: with the rehearsal's token endpoint the Entra ID smoke test runs instead.
    let entra_token_url = take_flag(&mut args, "--entra-token-url")
        .or_else(|| std::env::var("PUSH_SMOKE_ENTRA_TOKEN_URL").ok())
        .filter(|url| !url.is_empty());
    let target = match Target::parse(args, |name| std::env::var(name).ok()) {
        Ok(target) => target,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let smoke = async {
        match &entra_token_url {
            Some(url) => push_smoke::entra::run(&target, url).await,
            None => run(&target).await,
        }
    };
    match tokio::time::timeout(TOTAL_TIMEOUT, smoke).await {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            eprintln!("smoke: FAIL: {error:#}");
            ExitCode::FAILURE
        }
        Err(_) => {
            eprintln!("smoke: FAIL: not finished within {TOTAL_TIMEOUT:?}");
            ExitCode::FAILURE
        }
    }
}

/// Removes `flag` and its value from `args`, returning the value.
fn take_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == flag)?;
    args.remove(index);
    (index < args.len()).then(|| args.remove(index))
}
```

- [ ] **Step 4: The helm test.** Add the check:

```powershell
Check 'sign-in: the DEV ONLY smoke test runs in Entra mode with a token endpoint' {
    $rendered = helm template push $chart -f $minikube @signInArgs --set worker.email.webhook.enabled=false `
        --set smoke.entraTokenUrl=http://push-idp.push-idp.svc:8080/dev/token | Out-String
    (Doc $rendered 'Pod' 'push-smoke') -match '(?s)- --entra-token-url\s+- "http://push-idp.push-idp.svc:8080/dev/token"' -and
        (Doc (helm template push $chart -f $minikube | Out-String) 'Pod' 'push-smoke') -notmatch 'entra-token-url'
}
```

and in `templates\tests\smoke.yaml`, after the `--data-dir`/`/data` arguments:

```yaml
        {{- with .Values.smoke.entraTokenUrl }}
        - --entra-token-url
        - {{ . | quote }}
        {{- end }}
```

- [ ] **Step 5: Run.**

```powershell
cargo build -p push-smoke --locked
cargo +1.85 build --workspace --locked
cargo test -p push-smoke --locked
pwsh deploy/helm/test-chart.ps1
```

Expected: builds on stable and 1.85; push-smoke's tests (Plan 13A's `entra_sign_in`
included) pass; every chart check passes.

- [ ] **Step 6: Commit.**

```powershell
cargo fmt --all
git add -A
git commit -m "test: add a DEV ONLY Entra ID sign-in issuer and the smoke test's Entra mode"
```

### Task 7: The minikube rehearsal

Proves an Entra-only installation end to end without Entra ID: auth-svc trusting the
in-cluster HTTPS issuer through a CA bundle, under egress policies on Calico; no link page,
no email; the SDK smoke test signing in with Entra ID tokens.

**Files:**
- Create: `deploy\minikube\entra\sign-in-issuer.yaml`, `deploy\minikube\values-entra-sign-in-rehearsal.yaml`, `deploy\minikube\entra-sign-in-rehearsal.ps1`

- [ ] **Step 1: The issuer manifest.** Create `deploy\minikube\entra\sign-in-issuer.yaml`:

```yaml
# DEV ONLY: the Entra ID sign-in stub for the Plan 13B rehearsal. It mints tokens for
# anyone with a public test key: never outside a local cluster.
apiVersion: apps/v1
kind: Deployment
metadata:
  name: push-idp
  namespace: push-idp
  labels:
    app.kubernetes.io/name: push-idp
spec:
  replicas: 1
  selector:
    matchLabels:
      app.kubernetes.io/name: push-idp
  template:
    metadata:
      labels:
        app.kubernetes.io/name: push-idp
    spec:
      automountServiceAccountToken: false
      securityContext:
        runAsNonRoot: true
        runAsUser: 65532
        runAsGroup: 65532
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: issuer
          image: push-messenger/smoke:dev
          imagePullPolicy: IfNotPresent
          command: ["/usr/local/bin/push-dev-entra", "sign-in"]
          env:
            - name: PUSH_DEV_ENTRA_TLS_DIR
              value: /tls
            - name: PUSH_DEV_SIGN_IN_AUTHORITY
              value: https://push-idp.push-idp.svc/
          ports:
            - name: https
              containerPort: 8443
            - name: http
              containerPort: 8080
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: ["ALL"]
          resources:
            requests:
              cpu: 10m
              memory: 16Mi
            limits:
              cpu: 200m
              memory: 64Mi
          volumeMounts:
            - name: tls
              mountPath: /tls
              readOnly: true
      volumes:
        - name: tls
          secret:
            secretName: push-idp-tls
---
apiVersion: v1
kind: Service
metadata:
  name: push-idp
  namespace: push-idp
spec:
  selector:
    app.kubernetes.io/name: push-idp
  ports:
    - name: https
      port: 443
      targetPort: 8443
    - name: http
      port: 8080
      targetPort: 8080
```

- [ ] **Step 2: The values.** Create `deploy\minikube\values-entra-sign-in-rehearsal.yaml`:

```yaml
# DEV ONLY rehearsal of Entra ID-only sign-in on minikube (Plan 13B), layered after
# values-minikube.yaml. The issuer is deploy/minikube/entra/sign-in-issuer.yaml.
auth:
  signIn:
    providers: entra
    entra:
      clientIds:
        - 11111111-0000-4000-8000-000000000001
      tenants:
        - 33333333-0000-4000-8000-000000000003
      authorityHost: https://push-idp.push-idp.svc/
      caConfigMap: push-idp-ca
      maxSessionHours: 1
worker:
  email:
    # Entra-only writes no email: no webhook.
    webhook:
      enabled: false
smoke:
  entraTokenUrl: http://push-idp.push-idp.svc:8080/dev/token
networkPolicy:
  egress:
    enabled: true
    # Stands in for the Entra ID ranges: in-cluster Services are reached through pod IPs.
    extra:
      - to:
          - namespaceSelector:
              matchLabels:
                kubernetes.io/metadata.name: push-idp
```

(The test values 11111111-…/33333333-… are `push_auth::entra::testing::API_CLIENT_ID` and
`TENANT`, which the issuer's tokens carry.)

- [ ] **Step 3: The script.** Create `deploy\minikube\entra-sign-in-rehearsal.ps1`:

```powershell
# DEV ONLY rehearsal of Entra ID-only sign-in on minikube (Plan 13B): the chart with
# auth.signIn.providers=entra against an in-cluster HTTPS stub of Entra ID, egress
# policies on Calico, and the smoke test signing in with Entra ID tokens. Never touches
# the shared "push" profile; restores the kubectl context to "push" when done.
#
#   pwsh deploy/minikube/entra-sign-in-rehearsal.ps1 [-Cluster push13b-entra] [-SkipBuild] [-Keep]
param(
    [string] $Cluster = 'push13b-entra',
    [switch] $SkipBuild,
    [switch] $Keep
)
$ErrorActionPreference = 'Stop'
if ($Cluster -eq 'push') { throw 'Use an owned profile, never the shared "push" profile.' }
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$chart = Join-Path $root 'deploy\helm\push-messenger'
$certs = Join-Path ([System.IO.Path]::GetTempPath()) "push-idp-$([guid]::NewGuid())"

function Invoke-Step([string] $name, [scriptblock] $action) {
    Write-Host "== $name"
    & $action
    if ($LASTEXITCODE -and $LASTEXITCODE -ne 0) { throw "$name failed ($LASTEXITCODE)" }
}

function Apply-Yaml([string] $yaml) {
    $yaml | kubectl apply -f - | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'kubectl apply failed' }
}

try {
    $status = minikube status -p $Cluster --format '{{.Host}}' 2>$null
    if ($status -ne 'Running') {
        Invoke-Step 'start minikube with Calico' {
            minikube start -p $Cluster --driver=docker --cni=calico --cpus=4 --memory=6g
        }
        Invoke-Step 'enable the ingress addon' { minikube addons enable ingress -p $Cluster }
    }
    Invoke-Step 'use the profile' { kubectl config use-context $Cluster }
    if (-not $SkipBuild) {
        Invoke-Step 'build the images' { & (Join-Path $PSScriptRoot 'up.ps1') -Cluster $Cluster -SkipInstall }
    }
    Invoke-Step 'make the issuer''s certificate' {
        cargo run -p push-smoke --bin push-dev-entra -- certs $certs push-idp.push-idp.svc
    }
    foreach ($namespace in 'push-idp', 'push') {
        Apply-Yaml (kubectl create namespace $namespace --dry-run=client -o yaml | Out-String)
    }
    Apply-Yaml (kubectl create secret generic push-idp-tls -n push-idp `
        "--from-file=server.crt=$(Join-Path $certs 'server.crt')" `
        "--from-file=server.key=$(Join-Path $certs 'server.key')" --dry-run=client -o yaml | Out-String)
    Apply-Yaml (kubectl create configmap push-idp-ca -n push `
        "--from-file=ca.crt=$(Join-Path $certs 'ca.crt')" --dry-run=client -o yaml | Out-String)
    Invoke-Step 'deploy the issuer' { kubectl apply -f (Join-Path $PSScriptRoot 'entra\sign-in-issuer.yaml') }
    Invoke-Step 'wait for the issuer' { kubectl rollout status deployment/push-idp -n push-idp --timeout=180s }
    Invoke-Step 'install the chart, Entra ID only' {
        helm upgrade --install push $chart -n push `
            -f (Join-Path $chart 'values-minikube.yaml') `
            -f (Join-Path $PSScriptRoot 'values-entra-sign-in-rehearsal.yaml') `
            --set "buildId=$(Get-Date -Format 'yyyyMMddHHmmss')" --wait --timeout 10m
    }
    Invoke-Step 'run the smoke test in Entra mode' { helm test push -n push --logs }

    $auth = kubectl get deployment push-auth -n push -o json | ConvertFrom-Json
    $authEnv = $auth.spec.template.spec.containers[0].env
    if (($authEnv | Where-Object name -eq 'PUSH_AUTH_PROVIDERS').value -ne 'entra') { throw 'auth-svc is not Entra-only' }
    $worker = kubectl get deployment push-worker -n push -o json | ConvertFrom-Json
    if (($worker.spec.template.spec.containers[0].env | Where-Object name -eq 'PUSH_EMAIL_SENDER').value -ne 'none') {
        throw 'the worker still has an email sender'
    }
    kubectl get ingress push-auth-links -n push 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) { throw 'the link page Ingress exists' }
    $global:LASTEXITCODE = 0
    $logs = kubectl logs deployment/push-auth -n push --tail=-1 | Out-String
    if ($logs -match 'eyJ[A-Za-z0-9_-]{10,}\.') { throw 'a JWT reached auth-svc''s log' }
    if ($logs -notmatch 'auth.entra.provisioned') { throw 'no Entra ID account was provisioned' }
    Write-Host 'Entra ID sign-in rehearsal: PASS'
}
finally {
    Remove-Item -Recurse -Force $certs -ErrorAction SilentlyContinue
    kubectl config use-context push 2>$null | Out-Null
    if (-not $Keep) { minikube stop -p $Cluster | Out-Null }
}
```

- [ ] **Step 4: Run it.**

Run: `pwsh deploy/minikube/entra-sign-in-rehearsal.ps1`
Expected: every step passes and the script prints `Entra ID sign-in rehearsal: PASS`; the
smoke log shows "email and password sign-in is off", both sign-ins, the chat and "the same
device"; the context is `push` afterwards and the shared `push` profile is untouched. With
`-Keep`, also run `pwsh deploy/minikube/test-network-policy.ps1 -Context push13b-entra -Egress`
(12B Task 10): every check passes. Record the summary (durations, no tokens) in the PR body.

- [ ] **Step 5: Commit.**

```powershell
git add -A
git commit -m "test(minikube): rehearse Entra ID-only sign-in without Entra ID"
```

### Task 8: Verification and delivery

- [ ] **Step 1: Full checks.**

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 build --workspace --locked
cargo tree -i openssl-sys --workspace
cargo tree -i aws-lc-rs --workspace
pwsh deploy/helm/test-chart.ps1
helm template push deploy/helm/push-messenger > $env:TEMP\push-default-after-13b.yaml
git diff --no-index --stat $env:TEMP\push-default-before-13b.yaml $env:TEMP\push-default-after-13b.yaml
```

Expected: everything passes; neither `cargo tree` finds the package; the default render is
identical (no diff output, AC1).

- [ ] **Step 2: Supply chain.** `cargo deny check` (the verified release binary, as in PR #23).
Expected: `advisories ok, bans ok, licenses ok, sources ok`.

- [ ] **Step 3: Password mode on minikube.** `deploy/minikube/up.ps1 -Cluster push11b-local -Smoke`
on the existing volume: the upgrade keeps the data and the smoke test passes. Stop the
profile; restore the context to `push`.

- [ ] **Step 4: Status.** Roadmap row 13B `Done (#N)`; the spec's status line names 13A and
13B; README and `deployment-configuration.md` link the guide.

- [ ] **Step 5: Review and deliver.** Ask for a code review of `git diff main...HEAD`, fix
findings with tests, then push, open the PR, wait for CI, squash-merge with
`Microsoft Entra ID sign-in in the chart (Plan 13B) (#N)`, switch to `main`, pull, delete
the branch.

## Self-review against the spec

| Spec item | Task |
|---|---|
| D9 providers in values; Entra-only without email routes (service) and without email (worker `none`, no Resend keys, no link Ingress) | 1, 2, 3 |
| D3 tenants as ids; wildcards and personal accounts refused before install | 1 |
| D2 audiences include the application id; scope, role, client apps | 1, 2 |
| D4 egress to the authority host only from auth-svc; proxy; CA bundle | 2, 4 |
| D8 maximum session age as a value | 1, 2 |
| D14 builds on 12B's egress map and DEV ONLY tools | 4, 6, 7 |
| AC1 default render byte-identical; default smoke unchanged | 1 to 4 (checks), 8 |
| AC2 Entra-only installation signs in, registers and chats; no email anywhere | 3, 6, 7 |
| AC3 to AC7 | Plan 13A (service tests); 7 shows the issuer path in a cluster |
| Guide: the two app registrations, tenant restriction, browser flow, chart values, egress | Plan 13A Task 15, 5 |

Placeholder scan: no "TBD"; every template, script and code step is complete. Names used
across tasks: `push.entraSignIn`, `push.emailOff`, `push.mailboxFlag`, `push.checkSignIn`,
`push.signInEnv`, `push.signInCa`, `auth.signIn.*`, `networkPolicy.egress.entraSignInCidrs`,
`smoke.entraTokenUrl`, `push_smoke::entra::run`, `push-dev-entra sign-in`, and the 13A names
`StubOpenId::with_authority`, `user_claims`, `KEY_A`, `TENANT`, `sign`. Where 12B's merged
names differ (`serve_tls`, `$egressOf`, `$https`, the CA volume lists), the PR notes the
mapping.
