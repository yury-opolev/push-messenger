# Checks the Helm chart without a cluster: lint with each values file, the DEV ONLY
# switches, and the guards that keep dev mode out of real deployments.
#
#   pwsh deploy/helm/test-chart.ps1
$ErrorActionPreference = 'Stop'
$chart = Join-Path $PSScriptRoot 'push-messenger'
$minikube = Join-Path $chart 'values-minikube.yaml'
$failures = 0

function Check([string] $name, [scriptblock] $test) {
    if (& $test) {
        Write-Output "ok   $name"
    } else {
        Write-Output "FAIL $name"
        $script:failures++
    }
}

# The rendered document of this kind and name.
function Doc([string] $rendered, [string] $kind, [string] $name) {
    $rendered -split "(?m)^---\s*$" | Where-Object {
        $_ -match "(?m)^kind: $kind\s*$" -and $_ -match "(?m)^  name: $name\s*$"
    } | Select-Object -First 1
}

# Exit code of `helm template` with these arguments, output discarded.
function Render([string[]] $arguments) {
    helm template push $chart @arguments 2>&1 | Out-Null
    return $LASTEXITCODE
}

Check 'lint with the default values' { helm lint $chart --strict | Out-Null; $LASTEXITCODE -eq 0 }
Check 'lint with values-minikube.yaml' { helm lint $chart --strict -f $minikube | Out-Null; $LASTEXITCODE -eq 0 }

$default = helm template push $chart | Out-String
Check 'default: the gateway checks tokens with auth-svc''s keys, for its own URL, and has no dev identities' {
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $gateway -match 'name: PUSH_AUTH_URL\s+value: "http://push-auth:8080"' -and
        $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
        $default -notmatch 'PUSH_DEV_AUTH'
}
Check 'default: groups are refused; one switch serves them' {
    $on = helm template push $chart --set gateway.groups=true | Out-String
    (Doc $default 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "0"' -and
        (Doc $on 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "1"'
}
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
Check 'default: the worker has no dev mailbox and logs email; auth-svc has no mailbox at all' {
    $worker = Doc $default 'Deployment' 'push-worker'
    $auth = Doc $default 'Deployment' 'push-auth'
    $worker -match 'name: PUSH_DEV_MAILBOX\s+value: "0"' -and
        $worker -match 'name: PUSH_EMAIL_SENDER\s+value: "log"' -and $auth -notmatch 'PUSH_DEV_MAILBOX'
}
Check 'default: links use https and tokens name the gateway at wss' {
    $default -match 'name: PUSH_AUTH_PUBLIC_URL\s+value: "https://push.example.com"' -and
    $default -match 'name: PUSH_AUTH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"'
}
Check 'default: the only ConfigMap is the Postgres init scripts, without credentials' {
    $configMaps = @($default -split "(?m)^---\s*$" | Where-Object { $_ -match '(?m)^kind: ConfigMap' })
    $authPassword = [regex]::Match($default, 'auth-db-password: "([^"]+)"').Groups[1].Value
    $workerPassword = [regex]::Match($default, 'worker-db-password: "([^"]+)"').Groups[1].Value
    $configMaps.Count -eq 1 -and $configMaps[0] -match 'CREATE DATABASE push_auth' -and
        $configMaps[0] -match 'CREATE DATABASE push_worker' -and
        $configMaps[0] -match 'GRANT CONNECT ON DATABASE push_auth TO push_worker' -and
        $authPassword -and -not $configMaps[0].Contains($authPassword) -and
        $workerPassword -and -not $configMaps[0].Contains($workerPassword)
}
Check 'default: auth-svc and the worker get their own databases, the gateway none' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $worker = Doc $default 'Deployment' 'push-worker'
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $auth -match 'key: auth-database-url' -and $auth -notmatch 'key: database-url' -and
        $auth -match 'name: PUSH_AUTH_WORKER_ROLE\s+value: push_worker' -and
        $worker -match 'key: worker-database-url' -and $worker -match 'key: worker-auth-database-url' -and
        $worker -notmatch 'key: (auth-)?database-url' -and
        $gateway -match 'PUSH_MESSAGE_SVC_URL' -and $gateway -notmatch 'database-url'
}
Check 'default: message-svc and the worker share the Accounts token from the Secret, the gateway has none' {
    $messageSvc = Doc $default 'Deployment' 'push-message-svc'
    $worker = Doc $default 'Deployment' 'push-worker'
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $token = [regex]::Match($default, 'accounts-token: "([^"]+)"').Groups[1].Value
    $token.Length -eq 48 -and
        $messageSvc -match 'name: PUSH_ACCOUNTS_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: accounts-token' -and
        $worker -match 'name: PUSH_ACCOUNTS_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: accounts-token' -and
        $gateway -notmatch 'accounts-token' -and -not $messageSvc.Contains($token) -and -not $worker.Contains($token)
}
Check 'default: auth-svc and message-svc share the Directory token from the Secret, nobody else has it' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $messageSvc = Doc $default 'Deployment' 'push-message-svc'
    $token = [regex]::Match($default, 'directory-token: "([^"]+)"').Groups[1].Value
    $token.Length -eq 48 -and
        $auth -match 'name: PUSH_DIRECTORY_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: directory-token' -and
        $messageSvc -match 'name: PUSH_DIRECTORY_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: directory-token' -and
        $auth -match 'name: PUSH_MESSAGE_SVC_URL\s+value: "http://push-message-svc:50051"' -and
        $auth -notmatch 'accounts-token' -and
        (Doc $default 'Deployment' 'push-worker') -notmatch 'directory-token' -and
        (Doc $default 'Deployment' 'push-gateway') -notmatch 'directory-token' -and
        -not $auth.Contains($token) -and -not $messageSvc.Contains($token)
}
Check 'default: auth-svc and the worker drain like the gateway (preStop sleep, grace period)' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $worker = Doc $default 'Deployment' 'push-worker'
    $auth -match 'preStop:\s+sleep:\s+seconds: 5' -and $auth -match 'terminationGracePeriodSeconds: 30' -and
        $worker -match 'preStop:\s+sleep:\s+seconds: 5' -and $worker -match 'terminationGracePeriodSeconds: 30'
}
Check 'default: without a webhook secret there is no webhook Ingress' {
    -not (Doc $default 'Ingress' 'push-worker') -and $default -notmatch 'resend-webhook-secret:'
}
Check 'default: the ingress routes the auth API, not /dev, and keeps link tokens out of the access log' {
    $ingress = Doc $default 'Ingress' 'push-auth'
    $links = Doc $default 'Ingress' 'push-auth-links'
    $ingress -match 'path: /v1/auth\s' -and $ingress -match 'path: /v1/devices\s' -and
        $ingress -match 'path: /v1/users\s' -and $default -notmatch 'path: /dev' -and
        $ingress -notmatch 'enable-access-log' -and $links -match 'enable-access-log: "false"' -and
        $links -match 'path: /v1/auth/verify\s' -and $links -match 'path: /v1/auth/password/reset\s'
}

$webhook = helm template push $chart --set worker.email.webhookSecret=whsec_AAAAAAAAAAAAAAAAAAAAAAAA | Out-String
Check 'with a webhook secret: the worker''s Ingress routes /v1/webhooks/resend and nothing else' {
    $ingress = Doc $webhook 'Ingress' 'push-worker'
    $ingress -and ([regex]::Matches($ingress, '- path: ')).Count -eq 1 -and
        $ingress -match 'path: /v1/webhooks/resend\s+pathType: Exact' -and
        $webhook -match 'resend-webhook-secret: "whsec_AAAAAAAAAAAAAAAAAAAAAAAA"'
}
$resend = helm template push $chart --set worker.email.sender=resend --set worker.email.resendApiKey=re_test --set 'worker.email.from=Push <no-reply@push.example.com>' | Out-String
Check 'resend: the worker sends through Resend with the key from the Secret' {
    $worker = Doc $resend 'Deployment' 'push-worker'
    $worker -match 'name: PUSH_EMAIL_SENDER\s+value: "resend"' -and
        $worker -match 'key: resend-api-key' -and $resend -match 'resend-api-key: "re_test"' -and
        $worker -notmatch 're_test'
}

$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: no dev identities either; tokens name ws://push.local/v1/socket' {
    $dev -notmatch 'PUSH_DEV_AUTH' -and
        (Doc $dev 'Deployment' 'push-gateway') -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "ws://push.local/v1/socket"'
}
Check 'minikube: the smoke test calls message-svc and reads the Redis URL from the Secret' {
    $smoke = Doc $dev 'Pod' 'push-smoke'
    $smoke -match '- --message-svc\s+- "http://push-message-svc:50051"' -and
        $smoke -match 'name: PUSH_SMOKE_REDIS_URL\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: redis-url' -and
        $smoke -notmatch 'redis://' -and $dev -notmatch 'PUSH_DEV_SETUP'
}
Check 'minikube: the worker''s dev mailbox on' {
    (Doc $dev 'Deployment' 'push-worker') -match 'name: PUSH_DEV_MAILBOX\s+value: "1"'
}
Check 'minikube: a generated webhook secret and the webhook Ingress' {
    $dev -match 'resend-webhook-secret: "whsec_' -and (Doc $dev 'Ingress' 'push-worker')
}
Check 'minikube: links use http://push.local, tokens ws://push.local/v1/socket' {
    $dev -match 'name: PUSH_AUTH_PUBLIC_URL\s+value: "http://push.local"' -and
    $dev -match 'name: PUSH_AUTH_TOKEN_AUDIENCE\s+value: "ws://push.local/v1/socket"'
}
Check 'minikube: the smoke test is a helm test' { $dev -match 'helm.sh/hook: test' }
Check 'minikube: ingress timeouts are 120 s' { $dev -match 'proxy-read-timeout: "120"' -and $dev -match 'proxy-send-timeout: "120"' }

Check 'dev mode without the acknowledgement is refused' {
    (Render @('--set', 'devMode.enabled=true', '--set', 'ingress.host=push.local')) -ne 0
}
Check 'dev mode with a public ingress host is refused' {
    (Render @('-f', $minikube, '--set', 'ingress.host=chat.example.com')) -ne 0
}
Check 'plain http with a public ingress host is refused' {
    (Render @('--set', 'ingress.scheme=http')) -ne 0
}
Check 'resend without an API key or a From address is refused' {
    (Render @('--set', 'worker.email.sender=resend')) -ne 0 -and
    (Render @('--set', 'worker.email.sender=resend', '--set', 'worker.email.resendApiKey=re_test',
        '--set', 'worker.email.from=')) -ne 0
}
Check 'dev mode with the resend sender, or an unknown sender, is refused' {
    (Render @('-f', $minikube, '--set', 'worker.email.sender=resend', '--set', 'worker.email.resendApiKey=re_test',
        '--set', 'worker.email.from=a@push.local')) -ne 0 -and
    (Render @('--set', 'worker.email.sender=smtp')) -ne 0
}
Check 'dev mode with more than one worker is refused' {
    (Render @('-f', $minikube, '--set', 'worker.replicas=2')) -ne 0 -and
    (Render @('--set', 'worker.replicas=2')) -eq 0
}

if ($failures -gt 0) {
    Write-Output "$failures check(s) failed"
    exit 1
}
Write-Output 'all chart checks passed'
