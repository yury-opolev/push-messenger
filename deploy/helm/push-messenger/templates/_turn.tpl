{{/*
TURN for voice calls (calls.turn, docs/calls.md "TURN"): validation, auth-svc's variables,
and the helpers of the chart's coturn (coturn.yaml).
*/}}

{{/*
Checks a whole number: values files give float64, --set int64, --set-string a string
(refused). Compares as floats, since toString prints large float64 values as 1e+06.
Arguments: name, value, min, max.
*/}}
{{- define "push.checkInteger" -}}
{{- $value := .value -}}
{{- $valid := or (kindIs "float64" $value) (kindIs "int64" $value) (kindIs "int" $value) -}}
{{- if $valid -}}
{{- $valid = and (eq (float64 $value) (float64 (int64 $value))) (ge (int64 $value) (int64 .min)) (le (int64 $value) (int64 .max)) -}}
{{- end -}}
{{- if not $valid -}}
{{- fail (printf "%s must be an integer from %d to %d" .name (int64 .min) (int64 .max)) -}}
{{- end -}}
{{- end -}}

{{/* The 8 groups of an IPv6 address as comma-separated numbers; "" when it is not one. */}}
{{- define "push.ipv6Groups" -}}
{{- $address := lower (toString .) -}}
{{- if and (regexMatch `^[0-9a-f:]{2,39}$` $address) (not (contains ":::" $address)) (le (len (splitList "::" $address)) 2) -}}
{{- $halves := splitList "::" $address -}}
{{- $left := list -}}
{{- $right := list -}}
{{- with index $halves 0 -}}{{- $left = splitList ":" . -}}{{- end -}}
{{- $groups := $left -}}
{{- if eq (len $halves) 2 -}}
{{- with index $halves 1 -}}{{- $right = splitList ":" . -}}{{- end -}}
{{- $groups = list -}}
{{- if le (add (len $left) (len $right)) 7 -}}
{{- $groups = $left -}}
{{- range until (int (sub 8 (add (len $left) (len $right)))) -}}{{- $groups = append $groups "0" -}}{{- end -}}
{{- $groups = concat $groups $right -}}
{{- end -}}
{{- end -}}
{{- $valid := eq (len $groups) 8 -}}
{{- $numbers := list -}}
{{- range $groups -}}
{{- if regexMatch `^[0-9a-f]{1,4}$` . -}}
{{- $numbers = append $numbers (toString (int64 (printf "0x%s" .))) -}}
{{- else -}}
{{- $valid = false -}}
{{- end -}}
{{- end -}}
{{- if $valid -}}{{- join "," $numbers -}}{{- end -}}
{{- end -}}
{{- end -}}

{{/* "4" or "6" for an IPv4 or IPv6 address (no zone, no embedded IPv4); "" otherwise. */}}
{{- define "push.ipFamily" -}}
{{- $address := toString . -}}
{{- if regexMatch `^((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])$` $address -}}4
{{- else if include "push.ipv6Groups" $address -}}6
{{- end -}}
{{- end -}}

{{/* "true" for a DNS name (labels of letters, digits and inner hyphens, not all digits). */}}
{{- define "push.isDnsName" -}}
{{- $name := toString . -}}
{{- if and (le (len $name) 253) (regexMatch `^([A-Za-z0-9]([-A-Za-z0-9]{0,61}[A-Za-z0-9])?\.)*[A-Za-z0-9]([-A-Za-z0-9]{0,61}[A-Za-z0-9])?$` $name) (not (regexMatch `^[0-9.]+$` $name)) -}}true{{- end -}}
{{- end -}}

{{- define "push.ipv4String" -}}
{{- printf "%d.%d.%d.%d" (div . 16777216) (mod (div . 65536) 256) (mod (div . 256) 256) (mod . 256) -}}
{{- end -}}

{{/*
The inclusive range "<first>-<last>" of a CIDR, IPv4 or IPv6 (8 full hexadecimal groups),
the form coturn's denied-peer-ip takes (it aborts on CIDR notation); "" for anything but a
CIDR whose address is its network address.
*/}}
{{- define "push.cidrRange" -}}
{{- $parts := splitList "/" (toString .) -}}
{{- if and (eq (len $parts) 2) (regexMatch `^(0|[1-9][0-9]{0,2})$` (index $parts 1)) -}}
{{- $address := index $parts 0 -}}
{{- $prefix := int (index $parts 1) -}}
{{- $family := include "push.ipFamily" $address -}}
{{- if and (eq $family "4") (le $prefix 32) -}}
{{- $value := int64 0 -}}
{{- range splitList "." $address -}}{{- $value = add (mul $value 256) (int .) -}}{{- end -}}
{{- $size := int64 1 -}}
{{- range until (int (sub 32 $prefix)) -}}{{- $size = mul $size 2 -}}{{- end -}}
{{- if eq (mod $value $size) 0 -}}
{{- printf "%s-%s" (include "push.ipv4String" $value) (include "push.ipv4String" (add $value (sub $size 1))) -}}
{{- end -}}
{{- else if and (eq $family "6") (le $prefix 128) -}}
{{- $first := list -}}
{{- $last := list -}}
{{- $aligned := true -}}
{{- range $index, $group := splitList "," (include "push.ipv6Groups" $address) -}}
{{- $segment := int64 $group -}}
{{- $start := mul $index 16 -}}
{{- $bits := 0 -}}
{{- if le $prefix $start -}}
{{- $bits = 16 -}}
{{- else if lt $prefix (add $start 16) -}}
{{- $bits = sub (add $start 16) $prefix -}}
{{- end -}}
{{- $size := int64 1 -}}
{{- range until (int $bits) -}}{{- $size = mul $size 2 -}}{{- end -}}
{{- if ne (mod $segment $size) 0 -}}{{- $aligned = false -}}{{- end -}}
{{- $first = append $first (printf "%x" $segment) -}}
{{- $last = append $last (printf "%x" (add $segment (sub $size 1))) -}}
{{- end -}}
{{- if $aligned -}}
{{- printf "%s-%s" (join ":" $first) (join ":" $last) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* "true" when the chart deploys coturn. */}}
{{- define "push.coturnEnabled" -}}
{{- $turn := .Values.calls.turn -}}
{{- if and (eq (toString .Values.calls.enabled) "true") (eq (toString $turn.mode) "internal") (eq (toString $turn.internal.enabled) "true") -}}true{{- end -}}
{{- end -}}

{{/* The default application key auth-svc reads for calls.turn; "" when none. */}}
{{- define "push.turnSecretKey" -}}
{{- $turn := .Values.calls.turn -}}
{{- if eq (toString .Values.calls.enabled) "true" -}}
{{- if eq (toString $turn.mode) "internal" -}}turn-shared-secret
{{- else if eq (toString $turn.mode) "external" -}}
{{- $provider := toString $turn.external.provider -}}
{{- if eq $provider "generic" -}}turn-shared-secret
{{- else if eq $provider "cloudflare" -}}turn-cloudflare-token
{{- else if eq $provider "twilio" -}}turn-twilio-token
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* The Secret key auth-svc reads for calls.turn: the configured name or the default. */}}
{{- define "push.turnSecretRef" -}}
{{- $external := .Values.calls.turn.external -}}
{{- $default := include "push.turnSecretKey" . -}}
{{- $custom := "" -}}
{{- if eq (toString .Values.calls.turn.mode) "external" -}}
{{- $custom = get (dict "generic" $external.sharedSecret "cloudflare" $external.cloudflare.apiTokenSecret "twilio" $external.twilio.authTokenSecret) (toString $external.provider) -}}
{{- end -}}
{{- $custom | default $default -}}
{{- end -}}

{{/* publicHost as a URL host: an IPv6 address in brackets. */}}
{{- define "push.turnHost" -}}
{{- $host := toString .Values.calls.turn.internal.publicHost -}}
{{- if eq (include "push.ipFamily" $host) "6" -}}[{{ $host }}]{{- else -}}{{ $host }}{{- end -}}
{{- end -}}

{{/* The relay address coturn advertises: externalIp, loadBalancerIP, or an IP publicHost. */}}
{{- define "push.turnExternalIp" -}}
{{- $internal := .Values.calls.turn.internal -}}
{{- if $internal.externalIp -}}{{ $internal.externalIp }}
{{- else if $internal.service.loadBalancerIP -}}{{ $internal.service.loadBalancerIP }}
{{- else if include "push.ipFamily" $internal.publicHost -}}{{ $internal.publicHost }}
{{- end -}}
{{- end -}}

{{/* The peers coturn never relays to: files/coturn/denied-peers.txt, then deniedPeerCidrs. */}}
{{- define "push.turnDeniedCidrs" -}}
{{- $cidrs := list -}}
{{- range splitList "\n" (.Files.Get "files/coturn/denied-peers.txt") -}}
{{- $line := trim . -}}
{{- if and $line (not (hasPrefix "#" $line)) -}}{{- $cidrs = append $cidrs $line -}}{{- end -}}
{{- end -}}
{{- range .Values.calls.turn.internal.deniedPeerCidrs -}}{{- $cidrs = append $cidrs (toString .) -}}{{- end -}}
{{- join "\n" $cidrs -}}
{{- end -}}

{{/* "true" for an IPv6 CIDR whose address is IPv4-mapped (within ::ffff:0:0/96). */}}
{{- define "push.isIpv4MappedCidr" -}}
{{- $groups := splitList "," (include "push.ipv6Groups" (first (splitList "/" (toString .)))) -}}
{{- if and (eq (len $groups) 8) (eq (join "," (slice $groups 0 6)) "0,0,0,0,0,65535") -}}true{{- end -}}
{{- end -}}

{{/*
A valid CIDR in canonical form, as Go prints it: IPv6 in lower case without leading zeros,
the first longest run of two or more zero groups as "::" (RFC 5952). IPv4 is already
canonical (push.ipFamily refuses leading zeros).
*/}}
{{- define "push.cidrCanonical" -}}
{{- $parts := splitList "/" (toString .) -}}
{{- $address := index $parts 0 -}}
{{- if eq (include "push.ipFamily" $address) "6" -}}
{{- $groups := splitList "," (include "push.ipv6Groups" $address) -}}
{{- $bestStart := -1 -}}
{{- $bestLength := 1 -}}
{{- $runStart := -1 -}}
{{- range $index, $group := $groups -}}
{{- if eq $group "0" -}}
{{- if lt $runStart 0 -}}{{- $runStart = $index -}}{{- end -}}
{{- if gt (sub (add $index 1) $runStart) $bestLength -}}
{{- $bestStart = $runStart -}}
{{- $bestLength = sub (add $index 1) $runStart -}}
{{- end -}}
{{- else -}}
{{- $runStart = -1 -}}
{{- end -}}
{{- end -}}
{{- $left := list -}}
{{- $right := list -}}
{{- range $index, $group := $groups -}}
{{- $hex := printf "%x" (int64 $group) -}}
{{- if or (lt $bestStart 0) (lt $index $bestStart) -}}
{{- $left = append $left $hex -}}
{{- else if ge $index (add $bestStart $bestLength) -}}
{{- $right = append $right $hex -}}
{{- end -}}
{{- end -}}
{{- if lt $bestStart 0 -}}{{ join ":" $left }}{{- else -}}{{ join ":" $left }}::{{ join ":" $right }}{{- end -}}
/{{ index $parts 1 }}
{{- else -}}
{{- . -}}
{{- end -}}
{{- end -}}

{{/*
The denied peers as the coturn pod's egress NetworkPolicy excepts them: push.turnDeniedCidrs
in canonical form, without what Kubernetes refuses in ipBlock. The API server rejects
IPv4-mapped IPv6 CIDRs (::ffff:0:0/96 and within), and a pod never sends to such an address;
the IPv4 excepts under 0.0.0.0/0 cover the addresses they embed. coturn keeps denying them.
*/}}
{{- define "push.turnPolicyDeniedCidrs" -}}
{{- $cidrs := list -}}
{{- range splitList "\n" (include "push.turnDeniedCidrs" .) -}}
{{- if not (or (hasSuffix "/0" .) (include "push.isIpv4MappedCidr" .)) -}}
{{- $cidrs = append $cidrs (include "push.cidrCanonical" .) -}}
{{- end -}}
{{- end -}}
{{- join "\n" $cidrs -}}
{{- end -}}

{{/* coturn's configuration without the shared secret (the init container appends it). */}}
{{- define "push.coturnConfig" -}}
{{- $internal := .Values.calls.turn.internal -}}
{{- $relay := splitList "-" (toString $internal.relayPortRange) -}}
{{- .Files.Get "files/coturn/turnserver.conf" | replace "\r" "" | trimSuffix "\n" }}
# Set by the chart from calls.turn.internal.
realm={{ $internal.realm | default $internal.publicHost }}
external-ip={{ include "push.turnExternalIp" . }}
min-port={{ int (index $relay 0) }}
max-port={{ int (index $relay 1) }}
user-quota={{ int64 $internal.quotas.user }}
total-quota={{ int64 $internal.quotas.total }}
max-bps={{ int64 $internal.quotas.maxBps }}
{{- if $internal.tlsSecret }}
tls-listening-port=5349
cert=/etc/coturn/tls/tls.crt
pkey=/etc/coturn/tls/tls.key
{{- else }}
no-tls
{{- end }}
{{- range splitList "\n" (include "push.turnDeniedCidrs" .) }}
{{- with include "push.cidrRange" . }}
denied-peer-ip={{ . }}
{{- else }}
{{- fail (printf "files/coturn/denied-peers.txt or calls.turn.internal.deniedPeerCidrs holds %q, which is not a CIDR" .) }}
{{- end }}
{{- end }}
{{- end -}}

{{/* Hardened container settings for coturn (alpine image, uid 65534). */}}
{{- define "push.coturnSecurity" -}}
securityContext:
  runAsNonRoot: true
  runAsUser: 65534
  runAsGroup: 65534
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: ["ALL"]
  seccompProfile:
    type: RuntimeDefault
{{- end -}}

{{/* A Secret key name in a calls.turn.external field, never a secret value. */}}
{{- define "push.checkTurnKeyName" -}}
{{- if not (and (kindIs "string" .value) (or (not .value) (and (le (len .value) 63) (regexMatch `^[A-Za-z0-9._-]+$` .value)))) -}}
{{- fail (printf "%s must name a key of the Secret, not hold the secret" .name) -}}
{{- end -}}
{{- if and .value .volume -}}
{{- fail (printf "%s must be empty with secrets.volume: the file comes from secrets.files.%s" .name .default) -}}
{{- end -}}
{{- end -}}

{{/*
Checks calls.turn (docs/calls.md, "TURN"): the mode always, the rest only with calls on,
so a bad value stops the install instead of auth-svc or coturn.
*/}}
{{- define "push.checkTurn" -}}
{{- $calls := .Values.calls -}}
{{- $turn := $calls.turn -}}
{{- if not (and (kindIs "map" $turn) (kindIs "string" $turn.mode) (has $turn.mode (list "internal" "external" "stun-only"))) -}}
{{- fail "calls.turn.mode must be internal, external or stun-only" -}}
{{- end -}}
{{- if eq (toString $calls.enabled) "true" -}}
{{- include "push.checkInteger" (dict "name" "calls.turn.ttlSeconds" "value" $turn.ttlSeconds "min" 60 "max" 86400) -}}
{{- $stunError := "calls.turn.stunUrls must be a list of stun: or stuns: URLs" -}}
{{- if not (kindIs "slice" $turn.stunUrls) -}}{{- fail $stunError -}}{{- end -}}
{{- range $turn.stunUrls -}}
{{- if not (and (kindIs "string" .) (regexMatch `^stuns?:(\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9.-]+)(:[0-9]{1,5})?$` .)) -}}
{{- fail $stunError -}}
{{- end -}}
{{- end -}}
{{- if and (eq $turn.mode "stun-only") (not $turn.stunUrls) -}}
{{- fail "calls.turn.stunUrls must not be empty with calls.turn.mode stun-only" -}}
{{- end -}}
{{- if eq $turn.mode "internal" -}}
{{- $internal := $turn.internal -}}
{{- if not (kindIs "bool" $internal.enabled) -}}
{{- fail "calls.turn.internal.enabled must be true or false" -}}
{{- end -}}
{{- $host := $internal.publicHost -}}
{{- if not (and (kindIs "string" $host) (or (include "push.ipFamily" $host) (include "push.isDnsName" $host))) -}}
{{- fail "calls.turn.internal.publicHost must be the DNS name or IP address clients reach coturn at" -}}
{{- end -}}
{{- if not (and (kindIs "string" $internal.realm) (or (not $internal.realm) (include "push.isDnsName" $internal.realm))) -}}
{{- fail "calls.turn.internal.realm must be empty or a DNS-like name" -}}
{{- end -}}
{{- range $name := list "udp" "tcp" "tls" -}}
{{- include "push.checkInteger" (dict "name" (printf "calls.turn.internal.ports.%s" $name) "value" (index $internal.ports $name) "min" 1 "max" 65535) -}}
{{- end -}}
{{- $rangeError := "calls.turn.internal.relayPortRange must be \"<min>-<max>\" with 1024 <= min <= max <= 65535 and at most 500 ports" -}}
{{- if not (and (kindIs "string" $internal.relayPortRange) (regexMatch `^[1-9][0-9]{0,4}-[1-9][0-9]{0,4}$` $internal.relayPortRange)) -}}
{{- fail $rangeError -}}
{{- end -}}
{{- $relay := splitList "-" $internal.relayPortRange -}}
{{- $min := int (index $relay 0) -}}
{{- $max := int (index $relay 1) -}}
{{- if or (lt $min 1024) (gt $min $max) (gt $max 65535) (gt (sub $max $min) 499) -}}
{{- fail $rangeError -}}
{{- end -}}
{{- range $name := list "udp" "tcp" "tls" -}}
{{- $port := int (index $internal.ports $name) -}}
{{- if and (ge $port $min) (le $port $max) (or (ne $name "tls") $internal.tlsSecret) -}}
{{- fail (printf "calls.turn.internal.ports.%s must not be within calls.turn.internal.relayPortRange" $name) -}}
{{- end -}}
{{- end -}}
{{- if and $internal.tlsSecret (eq (int $internal.ports.tcp) (int $internal.ports.tls)) -}}
{{- fail "calls.turn.internal.ports.tls must differ from calls.turn.internal.ports.tcp" -}}
{{- end -}}
{{- $tls := $internal.tlsSecret -}}
{{- if not (and (kindIs "string" $tls) (or (not $tls) (and (le (len $tls) 253) (regexMatch `^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$` $tls)))) -}}
{{- fail "calls.turn.internal.tlsSecret must be empty or a Secret name" -}}
{{- end -}}
{{- $service := $internal.service -}}
{{- /* Not NodePort: node ports are 30000-32767, so neither the public ports nor coturn's
relay ports (advertised as external-ip:<port>) would be reachable at their numbers. */ -}}
{{- if not (and (kindIs "map" $service) (has $service.type (list "LoadBalancer" "ClusterIP"))) -}}
{{- fail "calls.turn.internal.service.type must be LoadBalancer or ClusterIP" -}}
{{- end -}}
{{- $annotationError := "calls.turn.internal.service.annotations must be a string-valued annotation map" -}}
{{- if not (kindIs "map" $service.annotations) -}}{{- fail $annotationError -}}{{- end -}}
{{- range $key, $value := $service.annotations -}}
{{- if not (kindIs "string" $value) -}}{{- fail $annotationError -}}{{- end -}}
{{- end -}}
{{- if not (and (kindIs "string" $service.loadBalancerIP) (or (not $service.loadBalancerIP) (include "push.ipFamily" $service.loadBalancerIP))) -}}
{{- fail "calls.turn.internal.service.loadBalancerIP must be empty or an IP address" -}}
{{- end -}}
{{- if not (and (kindIs "string" $internal.externalIp) (or (not $internal.externalIp) (include "push.ipFamily" $internal.externalIp))) -}}
{{- fail "calls.turn.internal.externalIp must be empty or an IP address" -}}
{{- end -}}
{{- if not (kindIs "map" $internal.resources) -}}
{{- fail "calls.turn.internal.resources must be a map of Kubernetes container resources" -}}
{{- end -}}
{{- include "push.checkInteger" (dict "name" "calls.turn.internal.quotas.user" "value" $internal.quotas.user "min" 1 "max" 1000) -}}
{{- include "push.checkInteger" (dict "name" "calls.turn.internal.quotas.total" "value" $internal.quotas.total "min" 1 "max" 100000) -}}
{{- include "push.checkInteger" (dict "name" "calls.turn.internal.quotas.maxBps" "value" $internal.quotas.maxBps "min" 8000 "max" 1000000000) -}}
{{- if not (and (kindIs "string" $internal.image) (regexMatch `^[a-z0-9]([a-z0-9.-]*[a-z0-9])?(:[0-9]+)?(/[a-z0-9]+([._-]+[a-z0-9]+)*)*:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}@sha256:[a-f0-9]{64}$` $internal.image)) -}}
{{- fail "calls.turn.internal.image must be <repository>:<tag>@sha256:<64 hex digits>" -}}
{{- end -}}
{{- if not (kindIs "bool" $internal.copyBinary) -}}
{{- fail "calls.turn.internal.copyBinary must be true or false" -}}
{{- end -}}
{{- $peerError := "calls.turn.internal.metricsPeers requires namespaceSelector and podSelector as non-empty label maps on every peer" -}}
{{- if not (kindIs "slice" $internal.metricsPeers) -}}{{- fail $peerError -}}{{- end -}}
{{- range $internal.metricsPeers -}}
{{- if not (and (kindIs "map" .) (kindIs "map" .namespaceSelector) .namespaceSelector (kindIs "map" .podSelector) .podSelector) -}}
{{- fail $peerError -}}
{{- end -}}
{{- end -}}
{{- if $internal.enabled -}}
{{- if not (include "push.turnExternalIp" .) -}}
{{- fail "calls.turn.internal.externalIp must be set (or calls.turn.internal.service.loadBalancerIP, or an IP address as publicHost): coturn advertises it as its relay address" -}}
{{- end -}}
{{- $cidrError := "calls.turn.internal.deniedPeerCidrs must list the cluster's pod and service CIDRs" -}}
{{- if not (and (kindIs "slice" $internal.deniedPeerCidrs) $internal.deniedPeerCidrs) -}}
{{- fail $cidrError -}}
{{- end -}}
{{- range $internal.deniedPeerCidrs -}}
{{- $range := "" -}}
{{- if kindIs "string" . -}}{{- $range = include "push.cidrRange" . -}}{{- end -}}
{{- $prefix := 0 -}}
{{- if $range -}}{{- $prefix = int (last (splitList "/" .)) -}}{{- end -}}
{{- /* coturn reads a range starting at the unspecified address :: as every address
(IPv4 too), and /0 would deny every peer. */ -}}
{{- if or (not $range) (and (not (contains ":" .)) (lt $prefix 8)) (and (contains ":" .) (or (lt $prefix 16) (hasPrefix "0:0:0:0:0:0:0:0-" $range))) -}}
{{- fail (printf "%s (network addresses such as 10.244.0.0/16 or fd00:10:244::/56; at least /8 for IPv4, /16 for IPv6, not starting at ::), not %v" $cidrError .) -}}
{{- end -}}
{{- /* Kubernetes refuses them in the NetworkPolicy's ipBlock; denied-peers.txt already
denies ::ffff:0:0/96 to coturn. */ -}}
{{- if include "push.isIpv4MappedCidr" . -}}
{{- fail (printf "calls.turn.internal.deniedPeerCidrs must not hold IPv4-mapped IPv6 CIDRs (within ::ffff:0:0/96; Kubernetes refuses them in NetworkPolicy ipBlock): list the IPv4 CIDR instead, not %v" .) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- else if eq $turn.mode "external" -}}
{{- $external := $turn.external -}}
{{- if not (and (kindIs "map" $external) (kindIs "string" $external.provider) (has $external.provider (list "generic" "cloudflare" "twilio"))) -}}
{{- fail "calls.turn.external.provider must be generic, cloudflare or twilio" -}}
{{- end -}}
{{- if eq $external.provider "generic" -}}
{{- $urlError := "calls.turn.external.urls must be a non-empty list of turn: or turns: URLs" -}}
{{- if not (and (kindIs "slice" $external.urls) $external.urls) -}}{{- fail $urlError -}}{{- end -}}
{{- range $external.urls -}}
{{- if not (and (kindIs "string" .) (regexMatch `^turns?:(\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9.-]+)(:[0-9]{1,5})?(\?transport=(udp|tcp))?$` .)) -}}
{{- fail $urlError -}}
{{- end -}}
{{- end -}}
{{- else if eq $external.provider "cloudflare" -}}
{{- if not (regexMatch `^[A-Za-z0-9]{1,64}$` (toString $external.cloudflare.keyId)) -}}
{{- fail "calls.turn.external.cloudflare.keyId must be set to the TURN key id (1 to 64 letters and digits)" -}}
{{- end -}}
{{- else if not (regexMatch `^AC[0-9a-fA-F]{32}$` (toString $external.twilio.accountSid)) -}}
{{- fail "calls.turn.external.twilio.accountSid must be AC followed by 32 hexadecimal digits" -}}
{{- end -}}
{{- $volume := .Values.secrets.volume -}}
{{- include "push.checkTurnKeyName" (dict "name" "calls.turn.external.sharedSecret" "value" $external.sharedSecret "volume" $volume "default" "turn-shared-secret") -}}
{{- include "push.checkTurnKeyName" (dict "name" "calls.turn.external.cloudflare.apiTokenSecret" "value" $external.cloudflare.apiTokenSecret "volume" $volume "default" "turn-cloudflare-token") -}}
{{- include "push.checkTurnKeyName" (dict "name" "calls.turn.external.twilio.authTokenSecret" "value" $external.twilio.authTokenSecret "volume" $volume "default" "turn-twilio-token") -}}
{{- if not (or .Values.secrets.existingSecret .Values.secrets.volume) -}}
{{- fail "calls.turn.mode external needs the provider's secret in a consumer-managed Secret: set secrets.existingSecret or secrets.volume" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* auth-svc's TURN variables (docs/calls.md, "TURN"), as env list entries; nothing with calls off. */}}
{{- define "push.turnEnv" -}}
{{- if eq (toString .Values.calls.enabled) "true" -}}
{{- include "push.checkTurn" . -}}
{{- $turn := .Values.calls.turn -}}
{{- $stun := list -}}
{{- $urls := list -}}
{{- if eq $turn.mode "internal" -}}
{{- $host := include "push.turnHost" . -}}
{{- $ports := $turn.internal.ports -}}
{{- $stun = list (printf "stun:%s:%d" $host (int64 $ports.udp)) -}}
{{- $urls = list (printf "turn:%s:%d?transport=udp" $host (int64 $ports.udp)) (printf "turn:%s:%d?transport=tcp" $host (int64 $ports.tcp)) -}}
{{- if $turn.internal.tlsSecret -}}
{{- $urls = append $urls (printf "turns:%s:%d?transport=tcp" $host (int64 $ports.tls)) -}}
{{- end -}}
{{- else if and (eq $turn.mode "external") (eq $turn.external.provider "generic") -}}
{{- $urls = $turn.external.urls -}}
{{- end -}}
{{- $stun = concat $stun $turn.stunUrls -}}
- name: PUSH_TURN_MODE
  value: {{ $turn.mode | quote }}
- name: PUSH_TURN_TTL_SECONDS
  value: {{ int64 $turn.ttlSeconds | quote }}
{{- with $stun }}
- name: PUSH_TURN_STUN_URLS
  value: {{ join "," . | quote }}
{{- end }}
{{- with $urls }}
- name: PUSH_TURN_URLS
  value: {{ join "," . | quote }}
{{- end }}
{{- if eq $turn.mode "external" }}
- name: PUSH_TURN_PROVIDER
  value: {{ $turn.external.provider | quote }}
{{- end }}
{{- if or (eq $turn.mode "internal") (and (eq $turn.mode "external") (eq $turn.external.provider "generic")) }}
{{- include "push.applicationEnv" (dict "root" . "name" "PUSH_TURN_SHARED_SECRET" "key" (include "push.turnSecretRef" .)) }}
{{- end }}
{{- if and (eq $turn.mode "external") (eq $turn.external.provider "cloudflare") }}
- name: PUSH_TURN_CLOUDFLARE_KEY_ID
  value: {{ $turn.external.cloudflare.keyId | quote }}
{{- include "push.applicationEnv" (dict "root" . "name" "PUSH_TURN_CLOUDFLARE_TOKEN" "key" (include "push.turnSecretRef" .)) }}
{{- end }}
{{- if and (eq $turn.mode "external") (eq $turn.external.provider "twilio") }}
- name: PUSH_TURN_TWILIO_ACCOUNT_SID
  value: {{ $turn.external.twilio.accountSid | quote }}
{{- include "push.applicationEnv" (dict "root" . "name" "PUSH_TURN_TWILIO_TOKEN" "key" (include "push.turnSecretRef" .)) }}
{{- end }}
{{- end -}}
{{- end -}}
