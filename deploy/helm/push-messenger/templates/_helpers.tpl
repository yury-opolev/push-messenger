{{/* Resource names, prefixed with the release name. */}}
{{- define "push.name" -}}
{{- printf "%s-%s" .root.Release.Name .component | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "push.labels" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/part-of: push-messenger
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .root.Chart.Name .root.Chart.Version }}
{{- end -}}

{{- define "push.selector" -}}
app.kubernetes.io/name: {{ .component }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
{{- end -}}

{{- define "push.secretName" -}}
{{- printf "%s-secrets" .Release.Name -}}
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
"true" when the worker gets a webhook secret, so it serves /v1/webhooks/resend and the
webhook Ingress is rendered: set in values, kept in the existing Secret, or generated in
dev mode (secret.yaml).
*/}}
{{- define "push.hasWebhook" -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- if or .Values.worker.email.webhookSecret (hasKey $existing "resend-webhook-secret") (include "push.devMode" .) -}}
true
{{- end -}}
{{- end -}}

{{/*
Checks the worker's email settings: the sender is log or resend; resend needs an API key
and a From address and is refused in dev mode, whose mailbox shows every email it sent.
Dev mode needs exactly one worker: its mailbox is in memory, per replica, so with more
the smoke test would look in the wrong one.
*/}}
{{- define "push.checkEmail" -}}
{{- $email := .Values.worker.email -}}
{{- if and (include "push.devMode" .) (gt (int .Values.worker.replicas) 1) -}}
{{- fail "dev mode keeps the DEV ONLY mailbox in memory, per worker replica: use worker.replicas 1 with it" -}}
{{- end -}}
{{- if not (has $email.sender (list "log" "resend")) -}}
{{- fail (printf "worker.email.sender must be log or resend, not %q" $email.sender) -}}
{{- end -}}
{{- if eq $email.sender "resend" -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- if not (or $email.resendApiKey (hasKey $existing "resend-api-key")) -}}
{{- fail "worker.email.sender resend needs worker.email.resendApiKey" -}}
{{- end -}}
{{- if not $email.from -}}
{{- fail "worker.email.sender resend needs worker.email.from" -}}
{{- end -}}
{{- if include "push.devMode" . -}}
{{- fail "dev mode shows every email the worker sends at /dev/mailbox: use worker.email.sender log with it" -}}
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
