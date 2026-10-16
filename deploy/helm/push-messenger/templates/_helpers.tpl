{{/* Resource names, prefixed with the release name. */}}
{{- define "push.name" -}}
{{- printf "%s-%s" .root.Release.Name .component | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "push.labels" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
push-messenger/network-peer: {{ .component }}
app.kubernetes.io/part-of: push-messenger
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end -}}

{{- define "push.selector" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
{{- end -}}

{{- define "push.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{- .Values.secrets.existingSecret -}}
{{- else -}}
{{- printf "%s-secrets" .Release.Name -}}
{{- end -}}
{{- end -}}

{{/*
A consumer-managed Secret (secrets.existingSecret) is only referenced: inline credential
values would be silently ignored, so they are refused by name, never by value.
*/}}
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

{{/* Checks a chart-managed auth key: canonical standard base64 of 32 (or at least 32) bytes. */}}
{{- define "push.checkAuthKey" -}}
{{- $decoded := .value | b64dec -}}
{{- if or (ne ($decoded | b64enc) .value) (and .exact (ne (len $decoded) 32)) (and (not .exact) (lt (len $decoded) 32)) -}}
{{- fail (printf "%s must be canonical standard base64 of %s" .name (ternary "exactly 32 bytes" "at least 32 bytes" .exact)) -}}
{{- end -}}
{{- end -}}

{{/*
The base URL clients reach the API at: email links start with it. Plain http only for a
local host, as auth-svc itself insists.
*/}}
{{- define "push.publicUrl" -}}
{{- if not (has .Values.ingress.scheme (list "https" "http")) -}}
{{- fail (printf "ingress.scheme must be https or http, not %q" .Values.ingress.scheme) -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
{{- if and (eq .Values.ingress.scheme "http") (not (or (hasSuffix ".local" $host) (hasSuffix ".localhost" $host) (hasSuffix ".test" $host))) -}}
{{- fail (printf "ingress.scheme http is only allowed with a local ingress host (*.local, *.localhost or *.test), not %q" $host) -}}
{{- end -}}
{{- printf "%s://%s" .Values.ingress.scheme .Values.ingress.host -}}
{{- end -}}

{{/* The gateway's WebSocket URL: the audience of access tokens. */}}
{{- define "push.socketUrl" -}}
{{- printf "%s://%s/v1/socket" (ternary "wss" "ws" (eq .Values.ingress.scheme "https")) .Values.ingress.host -}}
{{- end -}}

{{- define "push.image" -}}
{{- printf "%s:%s" .repository .tag -}}
{{- end -}}

{{/*
"true" when the DEV ONLY switches are on, "" otherwise. Rendering fails when
devMode.enabled is set without the acknowledgement, or with an ingress host that is not
local, so dev mode cannot reach a real deployment by accident.
*/}}
{{- define "push.devMode" -}}
{{- if .Values.devMode.enabled -}}
{{- if ne .Values.devMode.acknowledge "insecure-local-only" -}}
{{- fail "devMode.enabled serves every email the worker sends, links and all, without authentication: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
{{- if not (or (hasSuffix ".local" $host) (hasSuffix ".localhost" $host) (hasSuffix ".test" $host)) -}}
{{- fail (printf "devMode is only allowed with a local ingress host (*.local, *.localhost or *.test), not %q" $host) -}}
{{- end -}}
true
{{- end -}}
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_MAILBOX variable. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
{{- end -}}

{{/*
"true" when the worker serves /v1/webhooks/resend: an explicit decision
(worker.email.webhook.enabled), never inferred from a live Secret, so external-Secret
installations render offline. It references resend-webhook-secret and, with chart Ingress,
renders the webhook route.
*/}}
{{- define "push.hasWebhook" -}}
{{- if .Values.worker.email.webhook.enabled -}}true{{- end -}}
{{- end -}}

{{/*
Checks the worker's email settings: the sender is log or resend; resend needs a From
address and is refused in dev mode, whose mailbox shows every email it sent. Dev mode needs
at most one worker: its mailbox is in memory, per replica. Chart-managed credentials must
exist for resend and an enabled webhook; a consumer-managed Secret is never looked up (a
missing key stops the worker with a Kubernetes configuration error instead).
*/}}
{{- define "push.checkEmail" -}}
{{- $email := .Values.worker.email -}}
{{- if not (kindIs "bool" $email.webhook.enabled) -}}
{{- fail "worker.email.webhook.enabled must be a boolean" -}}
{{- end -}}
{{- if and (include "push.devMode" .) (gt (int .Values.worker.replicas) 1) -}}
{{- fail "dev mode keeps the DEV ONLY mailbox in memory, per worker replica: use worker.replicas 1 with it" -}}
{{- end -}}
{{- if not (has $email.sender (list "log" "resend")) -}}
{{- fail (printf "worker.email.sender must be log or resend, not %q" $email.sender) -}}
{{- end -}}
{{- if eq $email.sender "resend" -}}
{{- if not $email.from -}}
{{- fail "worker.email.sender resend needs worker.email.from" -}}
{{- end -}}
{{- if include "push.devMode" . -}}
{{- fail "dev mode shows every email the worker sends at /dev/mailbox: use worker.email.sender log with it" -}}
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

{{/* Hardened container settings for our own images (distroless, uid 65532). */}}
{{- define "push.containerSecurity" -}}
securityContext:
  runAsNonRoot: true
  runAsUser: 65532
  runAsGroup: 65532
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: ["ALL"]
  seccompProfile:
    type: RuntimeDefault
{{- end -}}
