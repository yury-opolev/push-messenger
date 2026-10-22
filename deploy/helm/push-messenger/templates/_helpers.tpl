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
{{- include "push.checkSecretVolume" . -}}
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

{{/* The volume is consumer-owned; only application-key consumers mount it. */}}
{{- define "push.checkSecretVolume" -}}
{{- $secrets := .Values.secrets -}}
{{- if not (kindIs "map" $secrets.volume) -}}
{{- fail "secrets.volume must be a Kubernetes VolumeSource object without name" -}}
{{- end -}}
{{- if $secrets.volume -}}
{{- if $secrets.existingSecret -}}
{{- fail "secrets.existingSecret conflicts with secrets.volume" -}}
{{- end -}}
{{- if or (ne (len $secrets.volume) 1) (hasKey $secrets.volume "name") -}}
{{- fail "secrets.volume must contain exactly one Kubernetes volume source, without name" -}}
{{- end -}}
{{- $sources := list "csi" "secret" "projected" "configMap" "nfs" "persistentVolumeClaim" "hostPath" "emptyDir" "ephemeral" "downwardAPI" "azureFile" "azureDisk" "cephfs" "cinder" "fc" "flexVolume" "flocker" "gcePersistentDisk" "gitRepo" "glusterfs" "iscsi" "photonPersistentDisk" "portworxVolume" "quobyte" "rbd" "scaleIO" "storageos" "vsphereVolume" "awsElasticBlockStore" -}}
{{- range $source, $spec := $secrets.volume -}}
{{- if or (not (has $source $sources)) (not (kindIs "map" $spec)) -}}
{{- fail "secrets.volume must contain a Kubernetes volume source object" -}}
{{- end -}}
{{- if eq $source "csi" -}}
{{- if not (and (kindIs "string" $spec.driver) $spec.driver) -}}
{{- fail "secrets.volume.csi.driver must be a nonempty string" -}}
{{- end -}}
{{- if and (hasKey $spec "volumeAttributes") (not (kindIs "map" $spec.volumeAttributes)) -}}
{{- fail "secrets.volume.csi.volumeAttributes must be a string map" -}}
{{- end -}}
{{- range $key, $value := $spec.volumeAttributes -}}
{{- if not (kindIs "string" $value) -}}
{{- fail "secrets.volume.csi.volumeAttributes must be a string map" -}}
{{- end -}}
{{- end -}}
{{- if and (eq $spec.driver "secrets-store.csi.k8s.io") (not (get ($spec.volumeAttributes | default dict) "secretProviderClass")) -}}
{{- fail "secrets.volume.csi.volumeAttributes.secretProviderClass is required" -}}
{{- end -}}
{{- end -}}
{{- $nameField := get (dict "secret" "secretName" "configMap" "name" "persistentVolumeClaim" "claimName" "hostPath" "path") $source -}}
{{- if $nameField -}}
{{- $value := get $spec $nameField -}}
{{- if not (and (kindIs "string" $value) $value) -}}
{{- fail (printf "secrets.volume.%s.%s must be a nonempty string" $source $nameField) -}}
{{- end -}}
{{- end -}}
{{- if eq $source "projected" -}}
{{- if not (and (kindIs "slice" $spec.sources) $spec.sources) -}}
{{- fail "secrets.volume.projected.sources must be a nonempty list" -}}
{{- end -}}
{{- range $spec.sources -}}
{{- if not (and (kindIs "map" .) .) -}}
{{- fail "secrets.volume.projected.sources must contain volume projection objects" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if not (and (kindIs "string" $secrets.mountPath) (regexMatch "^/([A-Za-z0-9_-][A-Za-z0-9._-]*)(/[A-Za-z0-9_-][A-Za-z0-9._-]*)*$" $secrets.mountPath)) -}}
{{- fail "secrets.mountPath must be an absolute non-root path without traversal or trailing slash" -}}
{{- end -}}
{{- if not (kindIs "map" $secrets.files) -}}
{{- fail "secrets.files must be a map of application key names to filenames" -}}
{{- end -}}
{{- $keys := list "auth-signing-key" "auth-link-key" "accounts-token" "directory-token" "messaging-token" "auth-admin-token" "resend-api-key" "resend-webhook-secret" -}}
{{- range $key, $filename := $secrets.files -}}
{{- if or (not (has $key $keys)) (not (kindIs "string" $filename)) -}}
{{- fail "secrets.files must contain only known application keys and string filenames" -}}
{{- end -}}
{{- if and $filename (or (not (regexMatch "^[A-Za-z0-9._-]+$" $filename)) (has $filename (list "." ".."))) -}}
{{- fail "secrets.files values must be filenames without traversal" -}}
{{- end -}}
{{- end -}}
{{- $required := list "auth-signing-key" "auth-link-key" "accounts-token" "directory-token" "messaging-token" -}}
{{- if .Values.auth.admin.enabled -}}{{- $required = append $required "auth-admin-token" -}}{{- end -}}
{{- if eq .Values.worker.email.sender "resend" -}}{{- $required = append $required "resend-api-key" -}}{{- end -}}
{{- if include "push.hasWebhook" . -}}{{- $required = append $required "resend-webhook-secret" -}}{{- end -}}
{{- range $required -}}
{{- if not (index $secrets.files .) -}}
{{- fail (printf "secrets.files.%s needs a filename" .) -}}
{{- end -}}
{{- end -}}
{{- range $key, $value := dict "auth.signingKey" .Values.auth.signingKey "auth.linkKey" .Values.auth.linkKey "worker.email.resendApiKey" .Values.worker.email.resendApiKey "worker.email.webhookSecret" .Values.worker.email.webhookSecret -}}
{{- if $value -}}{{- fail (printf "%s conflicts with secrets.volume" $key) -}}{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* Arguments: root, name (environment variable), key (existing Secret key). */}}
{{- define "push.applicationEnv" -}}
{{- if .root.Values.secrets.volume }}
- name: {{ .name }}_FILE
  value: {{ printf "%s/%s" .root.Values.secrets.mountPath (index .root.Values.secrets.files .key) | quote }}
{{- else }}
- name: {{ .name }}
  valueFrom:
    secretKeyRef:
      name: {{ include "push.secretName" .root }}
      key: {{ .key }}
{{- end }}
{{- end -}}

{{- define "push.applicationMounts" -}}
{{- if .Values.secrets.volume }}
- name: application-keys
  mountPath: {{ .Values.secrets.mountPath | quote }}
  readOnly: true
{{- end }}
{{- end -}}

{{- define "push.applicationVolumes" -}}
{{- if .Values.secrets.volume }}
- name: application-keys
  {{- toYaml .Values.secrets.volume | nindent 2 }}
{{- end }}
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

{{/* repository@digest when a digest is set, otherwise repository:tag. */}}
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
Checks the worker's email settings: the sender is log, resend or none; resend needs a From
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
{{- if not (has $email.sender (list "log" "resend" "none")) -}}
{{- fail (printf "worker.email.sender must be log, resend or none, not %q" $email.sender) -}}
{{- end -}}
{{- if and (eq $email.sender "none") $email.webhook.enabled -}}
{{- fail "worker.email.sender none sends no email: set worker.email.webhook.enabled false" -}}
{{- end -}}
{{- if eq $email.sender "resend" -}}
{{- if not $email.from -}}
{{- fail "worker.email.sender resend needs worker.email.from" -}}
{{- end -}}
{{- if include "push.devMode" . -}}
{{- fail "dev mode shows every email the worker sends at /dev/mailbox: use worker.email.sender log with it" -}}
{{- end -}}
{{- end -}}
{{- if not (or .Values.secrets.existingSecret .Values.secrets.volume) -}}
{{- $existing := (lookup "v1" "Secret" .Release.Namespace (include "push.secretName" .)).data | default dict -}}
{{- if and (eq $email.sender "resend") (not (or $email.resendApiKey (get $existing "resend-api-key"))) -}}
{{- fail "worker.email.sender resend needs worker.email.resendApiKey or a retained resend-api-key" -}}
{{- end -}}
{{- if and $email.webhook.enabled (not (include "push.devMode" .)) (not (or $email.webhookSecret (get $existing "resend-webhook-secret"))) -}}
{{- fail "worker.email.webhook.enabled needs worker.email.webhookSecret or a retained resend-webhook-secret" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* "true" when Microsoft Entra ID sign-in is offered (spec 2026-10-19 D9). */}}
{{- define "push.entraSignIn" -}}
{{- if has "entra" (splitList "," (nospace (toString .Values.auth.signIn.providers))) -}}true{{- end -}}
{{- end -}}

{{/* "true" when email and password sign-in is off: nothing ever writes an email then. */}}
{{- define "push.emailOff" -}}
{{- if not (has "password" (splitList "," (nospace (toString .Values.auth.signIn.providers)))) -}}true{{- end -}}
{{- end -}}

{{/* The worker's PUSH_EMAIL_SENDER: none whenever email and password sign-in is off. */}}
{{- define "push.emailSender" -}}
{{- if include "push.emailOff" . -}}none{{- else -}}{{ .Values.worker.email.sender }}{{- end -}}
{{- end -}}

{{/* "1" or "0" for PUSH_DEV_MAILBOX: dev mode, and only while the worker writes email. */}}
{{- define "push.mailboxFlag" -}}
{{- if eq (include "push.emailSender" .) "none" -}}0{{- else -}}{{ include "push.devFlag" . }}{{- end -}}
{{- end -}}

{{/*
Checks the sign-in settings (spec 2026-10-19 §3, D3, D9, D15) as auth-svc would at
start-up, so a bad value stops the install instead of the pods: the providers; with entra
the API's application id (optionally its api:// URI, at most four), the tenants (exactly
one of common, organizations, consumers, or one to twenty distinct tenant ids; never a
mixture), an https authority host, one scope or *, one role, client app ids, 1 to 720
session hours, an http(s) proxy URL. Entra-only installations write no email, so the
Resend sender and the webhook are refused with them, and the none sender without them.
The admin listener is a boolean; its peers carry both label maps.
*/}}
{{- define "push.checkSignIn" -}}
{{- $signIn := .Values.auth.signIn -}}
{{- $providerError := printf "auth.signIn.providers must be password, entra or password,entra, not %q" (toString $signIn.providers) -}}
{{- if not (kindIs "string" $signIn.providers) -}}
{{- fail $providerError -}}
{{- end -}}
{{- if not (has (nospace $signIn.providers) (list "password" "entra" "password,entra" "entra,password")) -}}
{{- fail $providerError -}}
{{- end -}}
{{- if not (kindIs "bool" .Values.auth.admin.enabled) -}}
{{- fail "auth.admin.enabled must be a boolean" -}}
{{- end -}}
{{- $adminPeerError := "networkPolicy.adminPeers requires namespaceSelector and podSelector as non-empty label maps on every peer" -}}
{{- if not (kindIs "slice" .Values.networkPolicy.adminPeers) -}}
{{- fail $adminPeerError -}}
{{- end -}}
{{- range .Values.networkPolicy.adminPeers -}}
{{- if not (and (kindIs "map" .) (kindIs "map" .namespaceSelector) .namespaceSelector (kindIs "map" .podSelector) .podSelector) -}}
{{- fail $adminPeerError -}}
{{- end -}}
{{- end -}}
{{- if include "push.entraSignIn" . -}}
{{- $entra := $signIn.entra -}}
{{- $guid := "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$" -}}
{{- range $name := list "clientIds" "tenants" "allowedClientApps" -}}
{{- if not (kindIs "slice" (index $entra $name)) -}}
{{- fail (printf "auth.signIn.entra.%s must be a list" $name) -}}
{{- end -}}
{{- end -}}
{{- if not $entra.clientIds -}}
{{- fail "auth.signIn.entra.clientIds needs the API registration's application id" -}}
{{- end -}}
{{- $audiences := list -}}
{{- $hasId := false -}}
{{- range $entra.clientIds -}}
{{- $id := toString . -}}
{{- /* PUSH_ENTRA_CLIENT_ID joins the entries with commas and auth-svc splits on them. */ -}}
{{- if contains "," $id -}}
{{- fail (printf "auth.signIn.entra.clientIds: %q holds a comma; give one audience per entry" $id) -}}
{{- end -}}
{{- if regexMatch $guid $id -}}
{{- $hasId = true -}}
{{- $audiences = append $audiences (lower $id) -}}
{{- else if regexMatch "^api://[!-~]{1,250}$" $id -}}
{{- $audiences = append $audiences $id -}}
{{- else -}}
{{- fail (printf "auth.signIn.entra.clientIds: %q is neither an application id nor an api:// URI" $id) -}}
{{- end -}}
{{- end -}}
{{- if not $hasId -}}
{{- fail "auth.signIn.entra.clientIds must include the application id: v2.0 access tokens carry it as their audience" -}}
{{- end -}}
{{- if gt (len (uniq $audiences)) 4 -}}
{{- fail "auth.signIn.entra.clientIds: at most four audiences" -}}
{{- end -}}
{{- if not $entra.tenants -}}
{{- fail "auth.signIn.entra.tenants needs [common], [organizations], [consumers] or your tenant ids" -}}
{{- end -}}
{{- $keywords := list "common" "organizations" "consumers" -}}
{{- $tenants := list -}}
{{- range $entra.tenants -}}{{- $tenants = append $tenants (lower (toString .)) -}}{{- end -}}
{{- if not (and (eq (len $tenants) 1) (has (first $tenants) $keywords)) -}}
{{- range $tenants -}}
{{- if has . $keywords -}}
{{- fail "auth.signIn.entra.tenants: common, organizations and consumers stand alone, never combined" -}}
{{- end -}}
{{- if or (not (regexMatch $guid .)) (eq . "00000000-0000-0000-0000-000000000000") -}}
{{- fail (printf "auth.signIn.entra.tenants: %q is neither a tenant id nor common, organizations or consumers" .) -}}
{{- end -}}
{{- end -}}
{{- if ne (len $tenants) (len (uniq $tenants)) -}}
{{- fail "auth.signIn.entra.tenants lists a tenant id twice" -}}
{{- end -}}
{{- if gt (len $tenants) 20 -}}
{{- fail "auth.signIn.entra.tenants: at most 20 tenant ids" -}}
{{- end -}}
{{- end -}}
{{- range $entra.allowedClientApps -}}
{{- if not (regexMatch $guid (toString .)) -}}
{{- fail (printf "auth.signIn.entra.allowedClientApps: %q is not an application id" (toString .)) -}}
{{- end -}}
{{- end -}}
{{- if gt (len $entra.allowedClientApps) 20 -}}
{{- fail "auth.signIn.entra.allowedClientApps: at most 20 application ids" -}}
{{- end -}}
{{- if not (regexMatch "^https://[^/@?#\\s]+/?$" (toString $entra.authorityHost)) -}}
{{- fail "auth.signIn.entra.authorityHost must be https://host/ (scheme and host only)" -}}
{{- end -}}
{{- if not (regexMatch "^[!-~]{1,128}$" (toString $entra.requiredScope)) -}}
{{- fail "auth.signIn.entra.requiredScope must be one scope name or *" -}}
{{- end -}}
{{- if and $entra.requiredRole (not (regexMatch "^[!-~]{1,128}$" (toString $entra.requiredRole))) -}}
{{- fail "auth.signIn.entra.requiredRole must be one app role value" -}}
{{- end -}}
{{- $hours := int $entra.maxSessionHours -}}
{{- if or (lt $hours 1) (gt $hours 720) (ne (toString $hours) (toString $entra.maxSessionHours)) -}}
{{- fail "auth.signIn.entra.maxSessionHours must be a whole number of hours from 1 to 720" -}}
{{- end -}}
{{- if and $entra.httpsProxy (not (regexMatch "^https?://[^\\s]+$" (toString $entra.httpsProxy))) -}}
{{- fail "auth.signIn.entra.httpsProxy must be an http:// or https:// URL" -}}
{{- end -}}
{{- if and $entra.caConfigMap (not (regexMatch "^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$" (toString $entra.caConfigMap))) -}}
{{- fail "auth.signIn.entra.caConfigMap must be a ConfigMap name" -}}
{{- end -}}
{{- $authorityCa := .Values.azure.workloadIdentity.authorityCaConfigMap -}}
{{- if and $entra.caConfigMap (include "push.authorityCa" (dict "root" . "key" "auth")) (ne $entra.caConfigMap $authorityCa) -}}
{{- fail "auth-svc trusts one CA bundle (SSL_CERT_FILE): set auth.signIn.entra.caConfigMap to the same ConfigMap as azure.workloadIdentity.authorityCaConfigMap, holding every root auth-svc needs" -}}
{{- end -}}
{{- $egress := .Values.networkPolicy.egress -}}
{{- if $egress.enabled -}}
{{- if not (kindIs "slice" $egress.entraSignInCidrs) -}}
{{- fail "networkPolicy.egress.entraSignInCidrs must be a list of CIDRs" -}}
{{- end -}}
{{- range $egress.entraSignInCidrs -}}
{{- if not (and (kindIs "string" .) (regexMatch "^[0-9a-fA-F:.]+/[0-9]{1,3}$" .)) -}}
{{- fail (printf "networkPolicy.egress.entraSignInCidrs must hold CIDRs such as 20.190.128.0/18, not %v" .) -}}
{{- end -}}
{{- end -}}
{{- if not (or $egress.entraSignInCidrs $egress.httpsCidrs $egress.extra $entra.httpsProxy) -}}
{{- fail "Entra ID sign-in with networkPolicy.egress.enabled: auth-svc must reach the authority host (login.microsoftonline.com). Set networkPolicy.egress.entraSignInCidrs (the AzureActiveDirectory service tag ranges), httpsCidrs or extra rules, or auth.signIn.entra.httpsProxy with a rule to the proxy in extra" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if include "push.emailOff" . -}}
{{- if eq .Values.worker.email.sender "resend" -}}
{{- fail "auth.signIn.providers entra writes no email: use worker.email.sender log or none (the chart renders none)" -}}
{{- end -}}
{{- if .Values.worker.email.webhook.enabled -}}
{{- fail "auth.signIn.providers entra writes no email: set worker.email.webhook.enabled false" -}}
{{- end -}}
{{- else if eq .Values.worker.email.sender "none" -}}
{{- fail "worker.email.sender none sends no email, but password sign-in needs its verification and reset emails: use log or resend, or auth.signIn.providers entra" -}}
{{- end -}}
{{- end -}}

{{/*
"true" when the chart itself points a workload's SSL_CERT_FILE at Plan 12B's authority CA
bundle (injection chart). Arguments: root, key.
*/}}
{{- define "push.authorityCa" -}}
{{- $wi := .root.Values.azure.workloadIdentity -}}
{{- if and (include "push.anyEntra" .root) (eq $wi.injection "chart") (index $wi.clientIds .key) $wi.authorityCaConfigMap -}}true{{- end -}}
{{- end -}}

{{/*
"true" when auth-svc mounts the sign-in CA bundle. When Plan 12B's authority bundle already
sets its SSL_CERT_FILE, push.checkSignIn insists on the same ConfigMap and that mount serves.
*/}}
{{- define "push.signInCa" -}}
{{- if and (include "push.entraSignIn" .) .Values.auth.signIn.entra.caConfigMap (not (include "push.authorityCa" (dict "root" . "key" "auth"))) -}}true{{- end -}}
{{- end -}}

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
  value: {{ join "," $entra.tenants | lower | quote }}
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
  value: {{ int $entra.maxSessionHours | quote }}
{{- with $entra.httpsProxy }}
- name: HTTPS_PROXY
  value: {{ . | quote }}
- name: NO_PROXY
  value: "localhost,127.0.0.1,.svc,.cluster.local"
{{- end }}
{{- if include "push.signInCa" . }}
- name: SSL_CERT_FILE
  value: "/etc/push/entra-ca/ca.crt"
{{- end }}
{{- end }}
{{- end -}}

{{/* auth-svc's container mount of the sign-in CA bundle (push.signInCa). */}}
{{- define "push.signInMounts" -}}
{{- if include "push.signInCa" . }}
- name: entra-ca
  mountPath: /etc/push/entra-ca
  readOnly: true
{{- end }}
{{- end -}}

{{/* auth-svc's pod volume of the sign-in CA bundle (push.signInCa). */}}
{{- define "push.signInVolumes" -}}
{{- if include "push.signInCa" . }}
- name: entra-ca
  configMap:
    name: {{ .Values.auth.signIn.entra.caConfigMap }}
    items:
      - key: ca.crt
        path: ca.crt
{{- end }}
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

{{/*
Checks the public routing settings, also with ingress.enabled false (the public identity
still names email links and the token audience): HTTPS chart Ingress needs a TLS Secret,
annotation maps are string-valued, trusted client-IP headers are header names.
*/}}
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

{{/* The common annotations merged with a route's own; the route's keys win. */}}
{{- define "push.ingressAnnotations" -}}
{{- $annotations := mergeOverwrite (deepCopy .root.Values.ingress.annotations) .specific -}}
{{- with $annotations }}
annotations:
  {{- toYaml . | nindent 2 }}
{{- end -}}
{{- end -}}

{{/* "true" when the services reach an external PostgreSQL with Entra ID tokens. */}}
{{- define "push.entraPostgres" -}}
{{- if and (not .Values.postgres.enabled) (eq .Values.postgres.external.auth "azure-entra") -}}true{{- end -}}
{{- end -}}

{{/* "true" when the services reach an external Redis with Entra ID tokens. */}}
{{- define "push.entraRedis" -}}
{{- if and (not .Values.redis.enabled) (eq .Values.redis.external.auth "azure-entra") -}}true{{- end -}}
{{- end -}}

{{/* "true" when any datastore uses Entra ID: identities and ServiceAccounts are needed. */}}
{{- define "push.anyEntra" -}}
{{- if or (include "push.entraPostgres" .) (include "push.entraRedis" .) -}}true{{- end -}}
{{- end -}}

{{/* A PostgreSQL role or database name the services accept: 1-63 of A-Z a-z 0-9 _ - . @ */}}
{{- define "push.checkPrincipal" -}}
{{- if not (and (kindIs "string" .value) (regexMatch "^[A-Za-z0-9_.@-]{1,63}$" .value)) -}}
{{- fail (printf "%s must be an Entra principal name (1 to 63 ASCII letters, digits, _ - . or @)" .name) -}}
{{- end -}}
{{- end -}}

{{/*
Checks the datastore and identity settings (spec 2026-10-18). Bundled datastores ignore
the external blocks. An external datastore needs its address; azure-entra needs a role
per identity (PostgreSQL), a rediss:// URL without credentials (Redis) and a client id for
every identity that reaches it; password mode needs a consumer-managed Secret with the URLs.
*/}}
{{- define "push.checkDatastores" -}}
{{- $pg := .Values.postgres -}}
{{- $redis := .Values.redis -}}
{{- $wi := .Values.azure.workloadIdentity -}}
{{- range $name, $value := dict "postgres.enabled" $pg.enabled "redis.enabled" $redis.enabled "migrations.job.enabled" .Values.migrations.job.enabled "networkPolicy.egress.enabled" .Values.networkPolicy.egress.enabled -}}
{{- if not (kindIs "bool" $value) -}}
{{- fail (printf "%s must be a boolean" $name) -}}
{{- end -}}
{{- end -}}
{{- if not $pg.enabled -}}
{{- if not $pg.external.host -}}
{{- fail "postgres.enabled false needs postgres.external.host" -}}
{{- end -}}
{{- $port := int $pg.external.port -}}
{{- if or (lt $port 1) (gt $port 65535) -}}
{{- fail "postgres.external.port must be a TCP port (1 to 65535)" -}}
{{- end -}}
{{- if not (has $pg.external.auth (list "azure-entra" "password")) -}}
{{- fail (printf "postgres.external.auth must be azure-entra or password, not %q" (toString $pg.external.auth)) -}}
{{- end -}}
{{- if and (eq $pg.external.auth "password") (not .Values.secrets.existingSecret) -}}
{{- fail "postgres.external.auth password needs secrets.existingSecret holding the database URLs" -}}
{{- end -}}
{{- if eq $pg.external.auth "azure-entra" -}}
{{- range $key := list "messageSvc" "auth" "worker" -}}
{{- include "push.checkPrincipal" (dict "name" (printf "postgres.external.users.%s" $key) "value" (index $pg.external.users $key)) -}}
{{- end -}}
{{- range $key := list "messaging" "auth" "worker" -}}
{{- include "push.checkPrincipal" (dict "name" (printf "postgres.external.databases.%s" $key) "value" (index $pg.external.databases $key)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if not $redis.enabled -}}
{{- if not (has $redis.external.auth (list "azure-entra" "password")) -}}
{{- fail (printf "redis.external.auth must be azure-entra or password, not %q" (toString $redis.external.auth)) -}}
{{- end -}}
{{- if eq $redis.external.auth "azure-entra" -}}
{{- if not (regexMatch "^rediss://[^@/:]+(:[0-9]+)?/?$" $redis.external.url) -}}
{{- fail "redis.external.url must be rediss://host:port without credentials for azure-entra" -}}
{{- end -}}
{{- else if not .Values.secrets.existingSecret -}}
{{- fail "redis.external.auth password needs secrets.existingSecret holding redis-url" -}}
{{- end -}}
{{- end -}}
{{- if include "push.anyEntra" . -}}
{{- if not (has $wi.injection (list "webhook" "chart")) -}}
{{- fail (printf "azure.workloadIdentity.injection must be webhook or chart, not %q" (toString $wi.injection)) -}}
{{- end -}}
{{- if and (eq $wi.injection "chart") (not $wi.tenantId) -}}
{{- fail "azure.workloadIdentity.injection chart needs azure.workloadIdentity.tenantId" -}}
{{- end -}}
{{- $needs := list -}}
{{- if include "push.entraPostgres" . -}}{{- $needs = concat $needs (list "auth" "messageSvc" "worker") -}}{{- end -}}
{{- if include "push.entraRedis" . -}}{{- $needs = concat $needs (list "auth" "messageSvc" "gateway") -}}{{- end -}}
{{- range $key := uniq $needs -}}
{{- if not (index $wi.clientIds $key) -}}
{{- fail (printf "azure.workloadIdentity.clientIds.%s is required: that workload reaches an Entra ID datastore" $key) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- with .Values.networkPolicy.egress -}}
{{- if .enabled -}}
{{- range $name := list "postgresCidrs" "redisCidrs" "httpsCidrs" -}}
{{- $cidrs := index $.Values.networkPolicy.egress $name -}}
{{- if not (kindIs "slice" $cidrs) -}}
{{- fail (printf "networkPolicy.egress.%s must be a list of CIDRs" $name) -}}
{{- end -}}
{{- range $cidrs -}}
{{- if not (and (kindIs "string" .) (regexMatch "^[0-9a-fA-F:.]+/[0-9]{1,3}$" .)) -}}
{{- fail (printf "networkPolicy.egress.%s must hold CIDRs such as 10.1.0.4/32, not %v" $name .) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $dnsError := "networkPolicy.egress.dns requires namespaceSelector and podSelector as non-empty label maps on every peer" -}}
{{- if not (and (kindIs "slice" .dns) .dns) -}}
{{- fail $dnsError -}}
{{- end -}}
{{- range .dns -}}
{{- if not (and (kindIs "map" .) (kindIs "map" .namespaceSelector) .namespaceSelector (kindIs "map" .podSelector) .podSelector) -}}
{{- fail $dnsError -}}
{{- end -}}
{{- end -}}
{{- if not (kindIs "slice" .extra) -}}
{{- fail "networkPolicy.egress.extra must be a list of NetworkPolicyEgressRule objects" -}}
{{- end -}}
{{- range .extra -}}
{{- if not (kindIs "map" .) -}}
{{- fail "networkPolicy.egress.extra must be a list of NetworkPolicyEgressRule objects" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if .Values.migrations.job.enabled -}}
{{- if not (include "push.entraPostgres" .) -}}
{{- fail "migrations.job.enabled needs an external PostgreSQL with postgres.external.auth azure-entra" -}}
{{- end -}}
{{- include "push.checkPrincipal" (dict "name" "postgres.external.users.migrate" "value" $pg.external.users.migrate) -}}
{{- if not $wi.clientIds.migrate -}}
{{- fail "migrations.job.enabled needs azure.workloadIdentity.clientIds.migrate" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
A service's PostgreSQL variables: the URL from the Secret (bundled, or external password
mode), or the Entra ID variables of Plan 12A. Arguments: root, secretKey, database, user.
*/}}
{{- define "push.postgresEnv" -}}
{{- $root := .root -}}
{{- if include "push.entraPostgres" $root }}
- name: PUSH_POSTGRES_AUTH
  value: azure-entra
- name: PUSH_DATABASE_HOST
  value: {{ $root.Values.postgres.external.host | quote }}
- name: PUSH_DATABASE_PORT
  value: {{ $root.Values.postgres.external.port | quote }}
- name: PUSH_DATABASE_NAME
  value: {{ .database | quote }}
- name: PUSH_DATABASE_USER
  value: {{ .user | quote }}
{{- if $root.Values.postgres.external.caConfigMap }}
- name: PUSH_DATABASE_CA_FILE
  value: /etc/push/postgres-ca/ca.crt
{{- end }}
- name: PUSH_RUN_MIGRATIONS
  value: {{ ternary "false" "true" $root.Values.migrations.job.enabled | quote }}
{{- else }}
- name: PUSH_DATABASE_URL
  valueFrom:
    secretKeyRef:
      name: {{ include "push.secretName" $root }}
      key: {{ .secretKey }}
{{- end }}
{{- end -}}

{{/* A service's Redis variables: the URL from the Secret, or the Entra ID variables. */}}
{{- define "push.redisEnv" -}}
{{- if include "push.entraRedis" . }}
- name: PUSH_REDIS_AUTH
  value: azure-entra
- name: PUSH_REDIS_URL
  value: {{ .Values.redis.external.url | quote }}
{{- with .Values.redis.external.user }}
- name: PUSH_REDIS_USER
  value: {{ . | quote }}
{{- end }}
{{- if .Values.redis.external.caConfigMap }}
- name: PUSH_REDIS_CA_FILE
  value: /etc/push/redis-ca/ca.crt
{{- end }}
{{- else }}
- name: PUSH_REDIS_URL
  valueFrom:
    secretKeyRef:
      name: {{ include "push.secretName" . }}
      key: redis-url
{{- end }}
{{- end -}}

{{/* Pod label: the AKS webhook mutates only pods that opt in. Arguments: root, key. */}}
{{- define "push.identityLabels" -}}
{{- $root := .root -}}
{{- if and (include "push.anyEntra" $root) (eq $root.Values.azure.workloadIdentity.injection "webhook") (index $root.Values.azure.workloadIdentity.clientIds .key) }}
azure.workload.identity/use: "true"
{{- end }}
{{- end -}}

{{/* Pod spec line: the workload's ServiceAccount in Entra ID mode. Arguments: root, component. */}}
{{- define "push.serviceAccount" -}}
{{- if include "push.anyEntra" .root }}
serviceAccountName: {{ include "push.name" (dict "root" .root "component" .component) }}
{{- end }}
{{- end -}}

{{/* Container env: the AZURE_* variables when the chart injects the identity itself. */}}
{{- define "push.identityEnv" -}}
{{- $root := .root -}}
{{- $wi := $root.Values.azure.workloadIdentity -}}
{{- if and (include "push.anyEntra" $root) (eq $wi.injection "chart") (index $wi.clientIds .key) }}
- name: AZURE_CLIENT_ID
  value: {{ index $wi.clientIds .key | quote }}
- name: AZURE_TENANT_ID
  value: {{ $wi.tenantId | quote }}
- name: AZURE_FEDERATED_TOKEN_FILE
  value: /var/run/secrets/azure/tokens/azure-identity-token
- name: AZURE_AUTHORITY_HOST
  value: {{ $wi.authorityHost | quote }}
{{- if $wi.authorityCaConfigMap }}
- name: SSL_CERT_FILE
  value: /etc/push/authority-ca/ca.crt
{{- end }}
{{- end }}
{{- end -}}

{{/*
Container volume mounts: the projected token (injection chart) and the CA bundles of the
datastores this workload reaches. Arguments: root, key, postgres, redis (booleans).
*/}}
{{- define "push.identityMounts" -}}
{{- $root := .root -}}
{{- $wi := $root.Values.azure.workloadIdentity -}}
{{- if and (include "push.anyEntra" $root) (eq $wi.injection "chart") (index $wi.clientIds .key) }}
- name: azure-identity-token
  mountPath: /var/run/secrets/azure/tokens
  readOnly: true
{{- if $wi.authorityCaConfigMap }}
- name: authority-ca
  mountPath: /etc/push/authority-ca
  readOnly: true
{{- end }}
{{- end }}
{{- if and (include "push.entraPostgres" $root) $root.Values.postgres.external.caConfigMap .postgres }}
- name: postgres-ca
  mountPath: /etc/push/postgres-ca
  readOnly: true
{{- end }}
{{- if and (include "push.entraRedis" $root) $root.Values.redis.external.caConfigMap .redis }}
- name: redis-ca
  mountPath: /etc/push/redis-ca
  readOnly: true
{{- end }}
{{- end -}}

{{/* Pod volumes matching push.identityMounts. */}}
{{- define "push.identityVolumes" -}}
{{- $root := .root -}}
{{- $wi := $root.Values.azure.workloadIdentity -}}
{{- if and (include "push.anyEntra" $root) (eq $wi.injection "chart") (index $wi.clientIds .key) }}
- name: azure-identity-token
  projected:
    sources:
      - serviceAccountToken:
          audience: api://AzureADTokenExchange
          expirationSeconds: 3600
          path: azure-identity-token
{{- if $wi.authorityCaConfigMap }}
- name: authority-ca
  configMap:
    name: {{ $wi.authorityCaConfigMap }}
{{- end }}
{{- end }}
{{- if and (include "push.entraPostgres" $root) $root.Values.postgres.external.caConfigMap .postgres }}
- name: postgres-ca
  configMap:
    name: {{ $root.Values.postgres.external.caConfigMap }}
{{- end }}
{{- if and (include "push.entraRedis" $root) $root.Values.redis.external.caConfigMap .redis }}
- name: redis-ca
  configMap:
    name: {{ $root.Values.redis.external.caConfigMap }}
{{- end }}
{{- end -}}

{{/* One `migrate` step of the migration Job (migrate-job.yaml). Arguments: root, step. */}}
{{- define "push.migrateStep" -}}
{{- $root := .root -}}
{{- $step := .step -}}
- name: {{ $step.name }}
  image: {{ include "push.image" $step.image }}
  imagePullPolicy: {{ $root.Values.images.pullPolicy }}
  args: ["migrate"]
  {{- include "push.containerSecurity" $root | nindent 2 }}
  env:
    {{- with include "push.identityEnv" (dict "root" $root "key" "migrate") }}
    {{- . | trim | nindent 4 }}
    {{- end }}
    - name: PUSH_POSTGRES_AUTH
      value: azure-entra
    - name: PUSH_DATABASE_HOST
      value: {{ $root.Values.postgres.external.host | quote }}
    - name: PUSH_DATABASE_PORT
      value: {{ $root.Values.postgres.external.port | quote }}
    - name: PUSH_DATABASE_NAME
      value: {{ $step.database | quote }}
    - name: PUSH_DATABASE_USER
      value: {{ $root.Values.postgres.external.users.migrate | quote }}
    - name: PUSH_DATABASE_RUNTIME_ROLE
      value: {{ $step.runtime | quote }}
    {{- with $step.worker }}
    - name: PUSH_AUTH_WORKER_ROLE
      value: {{ . | quote }}
    {{- end }}
    {{- if $root.Values.postgres.external.caConfigMap }}
    - name: PUSH_DATABASE_CA_FILE
      value: /etc/push/postgres-ca/ca.crt
    {{- end }}
  {{- with include "push.identityMounts" (dict "root" $root "key" "migrate" "postgres" true "redis" false) }}
  volumeMounts:
    {{- . | trim | nindent 4 }}
  {{- end }}
  resources:
    requests:
      cpu: 50m
      memory: 64Mi
    limits:
      cpu: 500m
      memory: 256Mi
{{- end -}}
