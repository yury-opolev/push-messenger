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
{{- fail "devMode.enabled switches authentication off: set devMode.acknowledge to \"insecure-local-only\", and only in a local values file" -}}
{{- end -}}
{{- $host := .Values.ingress.host -}}
{{- if not (or (hasSuffix ".local" $host) (hasSuffix ".localhost" $host) (hasSuffix ".test" $host)) -}}
{{- fail (printf "devMode is only allowed with a local ingress host (*.local, *.localhost or *.test), not %q" $host) -}}
{{- end -}}
true
{{- end -}}
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH and PUSH_DEV_SETUP variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
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
