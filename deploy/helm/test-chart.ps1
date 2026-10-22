# Checks the Helm chart without a cluster: lint with each values file, the DEV ONLY
# switches, and the guards that keep dev mode out of real deployments.
# Requires Helm and Python with deploy/helm/requirements-test.txt for policy checks.
#
#   pwsh deploy/helm/test-chart.ps1
$ErrorActionPreference = 'Stop'
$chart = Join-Path $PSScriptRoot 'push-messenger'
$minikube = Join-Path $chart 'values-minikube.yaml'
$failures = 0
$python = Get-Command python, python3 -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1
if (-not $python) {
    throw 'Chart checks require Python 3 (python or python3) and deploy/helm/requirements-test.txt.'
}
& $python.Source -c 'import yaml' 2>$null
if ($LASTEXITCODE -ne 0) {
    throw "Chart checks require PyYAML. Install it with: `"$($python.Source)`" -m pip install --requirement deploy/helm/requirements-test.txt"
}

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

# Parse YAML with Python/PyYAML, so checks cover complete rules, not
# unrelated occurrences of ports and labels. These checks do not prove CNI enforcement.
function PolicyMatrix([string] $rendered, [string] $release, [array] $controllers = @(),
    [bool] $webhook = $false, [bool] $devMode = $false) {
    $validator = @'
import json, sys, yaml
case = json.load(sys.stdin)
docs = [d for d in yaml.safe_load_all(case["rendered"]) if d]
release = case["release"]
policies = {d["metadata"]["name"]: d for d in docs if d["kind"] == "NetworkPolicy"}
components = ["message-svc", "postgres", "redis", "auth", "gateway", "worker"]
assert sum(d["kind"] == "NetworkPolicy" for d in docs) == 6, "exactly six policy documents"
assert set(policies) == {f"{release}-{c}" for c in components}, "exactly six application policies"
def selector(component):
    return {"app.kubernetes.io/name": component, "app.kubernetes.io/instance": release}
def peer(component):
    return {"podSelector": {"matchLabels": {
        "app.kubernetes.io/instance": release, "push-messenger/network-peer": component}}}
def rule(port, peers):
    return {"ports": [{"protocol": "TCP", "port": port}], "from": peers}
controllers = [{"namespaceSelector": {"matchLabels": p["namespaceSelector"]},
                "podSelector": {"matchLabels": p["podSelector"]}} for p in case["controllers"]]
expected = {
    "message-svc": [rule(50051, [peer("gateway")]), rule(50052, [peer("worker")]),
                    rule(50053, [peer("auth")])],
    "postgres": [rule(5432, [peer(c) for c in ["message-svc", "auth", "worker"]])],
    "redis": [rule(6379, [peer(c) for c in ["message-svc", "auth", "gateway"]])],
    "auth": [rule(8080, [peer("gateway")] + controllers)],
    "gateway": [rule(8080, controllers)] if controllers else [],
    "worker": [],
}
worker_peers = (controllers if case["webhook"] else []) + ([peer("smoke")] if case["devMode"] else [])
if worker_peers:
    expected["worker"] = [rule(8080, worker_peers)]
for component in components:
    policy = policies[f"{release}-{component}"]
    assert policy["apiVersion"] == "networking.k8s.io/v1"
    # Source ordering has no meaning; compare the complete source sets per rule.
    for ingress_rule in policy["spec"].get("ingress", []) + expected[component]:
        ingress_rule["from"] = sorted(ingress_rule["from"], key=lambda p: json.dumps(p, sort_keys=True))
    assert policy["spec"] == {"podSelector": {"matchLabels": selector(component)},
                              "policyTypes": ["Ingress"], "ingress": expected[component]}, component
    resources = [d for d in docs if d["metadata"]["name"] == f"{release}-{component}"]
    service = next(d for d in resources if d["kind"] == "Service")
    workload = next(d for d in resources if d["kind"] in ["Deployment", "StatefulSet"])
    assert service["spec"]["selector"] == selector(component), f"{component} Service selector changed"
    assert workload["spec"]["selector"] == {"matchLabels": selector(component)}, f"{component} workload selector changed"
for doc in docs:
    if doc["kind"] not in ["Deployment", "StatefulSet", "Pod"]:
        continue
    labels = (doc["metadata"] if doc["kind"] == "Pod" else doc["spec"]["template"]["metadata"])["labels"]
    assert labels["push-messenger/network-peer"] == labels["app.kubernetes.io/name"]
    assert labels["app.kubernetes.io/instance"] == release
    # A later probe with only role+instance labels cannot become a Service endpoint.
    probe = {k: v for k, v in labels.items() if k != "app.kubernetes.io/name"}
    assert not selector(labels["app.kubernetes.io/name"]).items() <= probe.items()
'@
    $result = @{
        rendered = $rendered; release = $release; controllers = @($controllers)
        webhook = $webhook; devMode = $devMode
    } | ConvertTo-Json -Depth 10 -Compress | & $python.Source -c $validator 2>&1
    $passed = $LASTEXITCODE -eq 0
    if (-not $passed) { $result | ForEach-Object { Write-Host $_ } }
    return $passed
}

Check 'lint with the default values' { helm lint $chart --strict | Out-Null; $LASTEXITCODE -eq 0 }
Check 'lint with values-minikube.yaml' { helm lint $chart --strict -f $minikube | Out-Null; $LASTEXITCODE -eq 0 }

$default = helm template push $chart | Out-String
Check 'network: message-svc has separate release-scoped rules for all three API callers' {
    $policy = Doc $default 'NetworkPolicy' 'push-message-svc'
    $policy -and
        $policy -match 'port: 50051' -and $policy -match 'port: 50052' -and $policy -match 'port: 50053' -and
        (PolicyMatrix $default 'push')
}
Check 'network: default denies public traffic, has no egress rules, and preserves workload/Service selectors' {
    PolicyMatrix $default 'push'
}
Check 'network: explicit opt-out renders zero policies' {
    $off = helm template push $chart --set networkPolicy.enabled=false | Out-String
    $LASTEXITCODE -eq 0 -and $off -notmatch '(?m)^kind: NetworkPolicy\s*$'
}
$secondRelease = helm template other $chart --namespace another | Out-String
Check 'network: a second release targets and admits only its own instance' { PolicyMatrix $secondRelease 'other' }
$controllerPeers = @(
    @{ namespaceSelector = @{ 'example.com/namespace' = 'edge' }; podSelector = @{ 'example.com/controller' = 'public' } },
    @{ namespaceSelector = @{ 'example.com/namespace' = 'edge-private' }; podSelector = @{ 'example.com/controller' = 'private' } }
)
$peerJson = ConvertTo-Json -InputObject $controllerPeers -Depth 5 -Compress
$withControllers = helm template push $chart --set-json "networkPolicy.ingressControllerPeers=$peerJson" | Out-String
Check 'network: each controller peer ANDs both selectors; no webhook means no worker access or production mailbox' {
    PolicyMatrix $withControllers 'push' $controllerPeers
}
$withWebhookControllers = helm template push $chart --set-json "networkPolicy.ingressControllerPeers=$peerJson" --set worker.email.webhookSecret=whsec_test --set worker.email.webhook.enabled=true | Out-String
Check 'network: controller peers reach the worker only with webhook exposure' {
    PolicyMatrix $withWebhookControllers 'push' $controllerPeers $true
}
$withoutIngress = helm template push $chart --set-json "networkPolicy.ingressControllerPeers=$peerJson" --set worker.email.webhookSecret=whsec_test --set worker.email.webhook.enabled=true --set ingress.enabled=false | Out-String
Check 'network: enabled webhook admits configured consumer routing peers without chart Ingress' {
    PolicyMatrix $withoutIngress 'push' $controllerPeers $true
}
foreach ($invalid in @(
    '[{}]',
    '[{"podSelector":{"app":"edge"}}]',
    '[{"namespaceSelector":{"team":"edge"}}]',
    '[{"namespaceSelector":{},"podSelector":{"app":"edge"}}]',
    '[{"namespaceSelector":{"team":"edge"},"podSelector":{}}]',
    '[{"namespaceSelector":"edge","podSelector":{"app":"edge"}}]',
    '[{"namespaceSelector":{"team":"edge"},"podSelector":["edge"]}]'
)) {
    Check "network: invalid controller selector is refused: $invalid" {
        $errorText = helm template push $chart --set-json "networkPolicy.ingressControllerPeers=$invalid" 2>&1 | Out-String
        $LASTEXITCODE -ne 0 -and $errorText -match 'networkPolicy.ingressControllerPeers.*namespaceSelector.*podSelector.*non-empty.*maps'
    }
}
Check 'default: messaging has a dedicated alphanumeric credential and matching non-superuser URL' {
    $secret = Doc $default 'Secret' 'push-secrets'
    $password = [regex]::Match($secret, '(?m)^  messaging-db-password: "([A-Za-z0-9]+)"\s*$').Groups[1].Value
    $admin = [regex]::Match($secret, '(?m)^  postgres-password: "([^"]+)"\s*$').Groups[1].Value
    $password -and $admin -and $password -ne $admin -and
        $secret.Contains("database-url: `"postgres://push_messaging:${password}@push-postgres:5432/push_messaging`"") -and
        $secret -notmatch 'postgres://push:'
}
Check 'default: only bundled Postgres receives the bootstrap admin credential' {
    $postgres = Doc $default 'StatefulSet' 'push-postgres'
    $consumers = @($default -split "(?m)^---\s*$" | Where-Object {
        $_ -match '(?m)^kind: (Deployment|StatefulSet|Pod)\s*$' -and $_ -match 'key: postgres-password'
    })
    $consumers.Count -eq 1 -and $consumers[0] -eq $postgres -and
        $postgres -match 'name: POSTGRES_USER\s+value: push\s' -and
        $postgres -match 'name: POSTGRES_PASSWORD\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: postgres-password' -and
        $postgres -match 'name: PUSH_MESSAGING_DB_PASSWORD\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: messaging-db-password' -and
        (Doc $default 'Deployment' 'push-message-svc') -match 'key: database-url' -and
        (Doc $default 'Deployment' 'push-message-svc') -notmatch 'postgres-password|envFrom:'
}
Check 'default: fresh messaging bootstrap creates a plain login and transfers only its database' {
    $init = Doc $default 'ConfigMap' 'push-postgres-init'
    $init -match '(?s)05-push-messaging\.sh:.*CREATE ROLE push_messaging LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS.*ALTER DATABASE push_messaging OWNER TO push_messaging;.*10-push-auth\.sh:' -and
        $init.Contains('\getenv password PUSH_MESSAGING_DB_PASSWORD') -and
        $init -match '(?s)\\else\s+\\echo Invalid PUSH_MESSAGING_DB_PASSWORD.*?RAISE EXCEPTION' -and
        $init -notmatch 'CREATE SCHEMA messaging|messaging-role\.sql' -and
        -not $init.Contains([regex]::Match($default, 'messaging-db-password: "([^"]+)"').Groups[1].Value)
}
$messagingOverride = helm template push $chart --set-string postgres.messagingPassword=MessagingTest42 --set-string postgres.password=AdminTest24 | Out-String
Check 'explicit messaging password agrees with its URL without replacing the admin credential' {
    $secret = Doc $messagingOverride 'Secret' 'push-secrets'
    $secret -match 'messaging-db-password: "MessagingTest42"' -and
        $secret -match 'database-url: "postgres://push_messaging:MessagingTest42@push-postgres:5432/push_messaging"' -and
        $secret -match 'postgres-password: "AdminTest24"'
}
Check 'messaging passwords with URL delimiters are refused' {
    (Render @('--set-string', 'postgres.messagingPassword=bad:password')) -ne 0
}
Check 'default: message-svc exposes dedicated named gRPC Service ports' {
    $service = Doc $default 'Service' 'push-message-svc'
    ([regex]::Matches($service, '(?m)^\s+- name: ')).Count -eq 3 -and
        $service -match 'name: messaging\s+port: 50051\s+targetPort: messaging\s+appProtocol: grpc' -and
        $service -match 'name: accounts\s+port: 50052\s+targetPort: accounts\s+appProtocol: grpc' -and
        $service -match 'name: directory\s+port: 50053\s+targetPort: directory\s+appProtocol: grpc'
}
Check 'default: message-svc binds dedicated API listeners and preserves the health port' {
    $messageSvc = Doc $default 'Deployment' 'push-message-svc'
    ([regex]::Matches($messageSvc, 'containerPort: ')).Count -eq 4 -and
        $messageSvc -match 'name: messaging\s+containerPort: 50051' -and
        $messageSvc -match 'name: accounts\s+containerPort: 50052' -and
        $messageSvc -match 'name: directory\s+containerPort: 50053' -and
        $messageSvc -match 'name: health\s+containerPort: 8081' -and
        $messageSvc -match 'name: PUSH_GRPC_ADDR\s+value: "0.0.0.0:50051"' -and
        $messageSvc -match 'name: PUSH_ACCOUNTS_GRPC_ADDR\s+value: "0.0.0.0:50052"' -and
        $messageSvc -match 'name: PUSH_DIRECTORY_GRPC_ADDR\s+value: "0.0.0.0:50053"'
}
Check 'default: each message-svc caller uses its dedicated API port' {
    (Doc $default 'Deployment' 'push-gateway') -match 'name: PUSH_MESSAGE_SVC_URL\s+value: "http://push-message-svc:50051"' -and
        (Doc $default 'Deployment' 'push-worker') -match 'name: PUSH_MESSAGE_SVC_URL\s+value: "http://push-message-svc:50052"' -and
        (Doc $default 'Deployment' 'push-auth') -match 'name: PUSH_MESSAGE_SVC_URL\s+value: "http://push-message-svc:50053"'
}
Check 'default: the gateway checks tokens with auth-svc''s keys, for its own URL, and has no dev identities' {
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $gateway -match 'name: PUSH_AUTH_URL\s+value: "http://push-auth:8080"' -and
        $gateway -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "wss://push.example.com/v1/socket"' -and
        $gateway -notmatch 'PUSH_CLIENT_IP_HEADER' -and
        $default -notmatch 'PUSH_DEV_AUTH'
}
Check 'gateway.authUrl fetches the key set over https' {
    $tls = helm template push $chart --set gateway.authUrl=https://push-auth.push.svc:8443 | Out-String
    (Doc $tls 'Deployment' 'push-gateway') -match 'name: PUSH_AUTH_URL\s+value: "https://push-auth.push.svc:8443"'
}
Check 'default: the bundled PostgreSQL and Redis images are pinned by digest' {
    (Doc $default 'StatefulSet' 'push-postgres') -match 'image: postgres:17-alpine@sha256:[0-9a-f]{64}\s' -and
        (Doc $default 'Deployment' 'push-redis') -match 'image: redis:7-alpine@sha256:[0-9a-f]{64}\s'
}
Check 'default: PostgreSQL keeps only the capabilities its entrypoint needs, on a read-only root' {
    $postgres = Doc $default 'StatefulSet' 'push-postgres'
    $postgres -match 'allowPrivilegeEscalation: false' -and
        $postgres -match 'readOnlyRootFilesystem: true' -and
        $postgres -match 'drop: \["ALL"\]\s+add: \["CHOWN", "DAC_OVERRIDE", "FOWNER", "SETGID", "SETUID"\]' -and
        $postgres -match 'mountPath: /var/run/postgresql' -and $postgres -match 'mountPath: /tmp' -and
        $postgres -notmatch 'fsGroup'
}
Check 'default: groups are refused; one switch serves them' {
    $on = helm template push $chart --set gateway.groups=true | Out-String
    (Doc $default 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "0"' -and
        (Doc $on 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "1"'
}
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' -and $default -notmatch 'PUSH_SMOKE_SIGNING_KEY' }
Check 'operator smoke: consumer Secret enables helm test without DEV credentials' {
    $rendered = helm template push $chart --set smoke.entraAccountsSecret=operator-accounts | Out-String
    $smoke = Doc $rendered 'Pod' 'push-smoke'
    $smoke -and $smoke -match '(?s)- --entra-accounts\s+- "/operator-accounts/accounts.jsonl"' -and
        $smoke -match 'secretName: "?operator-accounts"?' -and
        $smoke -match 'mountPath: /operator-accounts' -and
        $smoke -notmatch 'PUSH_SMOKE_SIGNING_KEY|--mailbox|--entra-token-url'
}
Check 'operator smoke: issuer input is mutually exclusive and accounts key is a filename' {
    (Render @('--set', 'smoke.entraAccountsSecret=operator-accounts',
        '--set', 'smoke.entraTokenUrl=http://stub/dev/token')) -ne 0 -and
    (Render @('--set', 'smoke.entraAccountsSecret=operator-accounts',
        '--set', 'smoke.entraAccountsKey=../accounts.jsonl')) -ne 0
}
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
        $auth -match 'name: PUSH_MESSAGE_SVC_URL\s+value: "http://push-message-svc:50053"' -and
        $auth -notmatch 'accounts-token' -and
        (Doc $default 'Deployment' 'push-worker') -notmatch 'directory-token' -and
        (Doc $default 'Deployment' 'push-gateway') -notmatch 'directory-token' -and
        -not $auth.Contains($token) -and -not $messageSvc.Contains($token)
}
Check 'default: the gateway and message-svc share the Messaging token from the Secret, nobody else has it' {
    $gateway = Doc $default 'Deployment' 'push-gateway'
    $messageSvc = Doc $default 'Deployment' 'push-message-svc'
    $token = [regex]::Match($default, 'messaging-token: "([^"]+)"').Groups[1].Value
    $accounts = [regex]::Match($default, 'accounts-token: "([^"]+)"').Groups[1].Value
    $directory = [regex]::Match($default, 'directory-token: "([^"]+)"').Groups[1].Value
    $token.Length -eq 48 -and $token -ne $accounts -and $token -ne $directory -and
        $gateway -match 'name: PUSH_MESSAGING_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: messaging-token' -and
        $messageSvc -match 'name: PUSH_MESSAGING_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: messaging-token' -and
        (Doc $default 'Deployment' 'push-auth') -notmatch 'messaging-token' -and
        (Doc $default 'Deployment' 'push-worker') -notmatch 'messaging-token' -and
        -not $gateway.Contains($token) -and -not $messageSvc.Contains($token)
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
Check 'default: the ingress routes the auth API, not /dev, with Exact link routes and no controller-specific annotations' {
    $ingress = Doc $default 'Ingress' 'push-auth'
    $links = Doc $default 'Ingress' 'push-auth-links'
    $ingress -match 'path: /v1/auth\s' -and $ingress -match 'path: /v1/devices\s' -and
        $ingress -match 'path: /v1/users\s' -and $default -notmatch 'path: /dev' -and
        $default -notmatch 'nginx\.ingress\.kubernetes\.io/' -and
        $links -match 'path: /v1/auth/verify\s+pathType: Exact' -and
        $links -match 'path: /v1/auth/password/reset\s+pathType: Exact'
}

$webhook = helm template push $chart --set worker.email.webhookSecret=whsec_AAAAAAAAAAAAAAAAAAAAAAAA --set worker.email.webhook.enabled=true | Out-String
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
Check 'network: minikube explicitly admits its local controller and same-release smoke mailbox peer' {
    $localPeers = @(@{
        namespaceSelector = @{ 'kubernetes.io/metadata.name' = 'ingress-nginx' }
        podSelector = @{ 'app.kubernetes.io/name' = 'ingress-nginx' }
    })
    PolicyMatrix $dev 'push' $localPeers $true $true
}
$devWithoutControllers = helm template push $chart -f $minikube --set-json 'networkPolicy.ingressControllerPeers=[]' | Out-String
Check 'network: dev mailbox admits smoke without a wildcard public fallback' {
    PolicyMatrix $devWithoutControllers 'push' @() $true $true
}
Check 'minikube: no dev identities either; tokens name ws://push.local/v1/socket' {
    $dev -notmatch 'PUSH_DEV_AUTH' -and
        (Doc $dev 'Deployment' 'push-gateway') -match 'name: PUSH_TOKEN_AUDIENCE\s+value: "ws://push.local/v1/socket"'
}
Check 'minikube: the smoke test reaches only the ingress and the mailbox, with auth-svc''s key from the Secret' {
    $smoke = Doc $dev 'Pod' 'push-smoke'
    $key = [regex]::Match($dev, 'auth-signing-key: "([^"]+)"').Groups[1].Value
    $smoke -notmatch 'message-svc' -and $smoke -notmatch 'redis' -and
        $smoke -match 'name: PUSH_SMOKE_SIGNING_KEY\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: auth-signing-key' -and
        $key -and -not $smoke.Contains($key) -and $dev -notmatch 'PUSH_DEV_SETUP'
}
Check 'minikube: the smoke test keeps the SDK apps'' databases in an emptyDir' {
    $smoke = Doc $dev 'Pod' 'push-smoke'
    $smoke -match '- --data-dir\s+- /data' -and
        $smoke -match 'volumeMounts:\s+- name: data\s+mountPath: /data' -and
        $smoke -match 'volumes:\s+- name: data\s+emptyDir:' -and
        $smoke -match 'readOnlyRootFilesystem: true'
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
Check 'minikube: helm test waits 6 minutes, longer than the smoke run''s 300 s' {
    $scripts = Join-Path $PSScriptRoot '../minikube'
    $main = Join-Path $PSScriptRoot '../../crates/push-smoke/src/main.rs'
    (Get-Content (Join-Path $scripts 'up.ps1') -Raw) -match 'helm test .* --timeout 6m' -and
        (Get-Content (Join-Path $scripts 'up.sh') -Raw) -match 'helm test .* --timeout 6m' -and
        (Get-Content $main -Raw) -match 'TOTAL_TIMEOUT: Duration = Duration::from_secs\(300\)'
}
Check 'minikube: ingress timeouts are 120 s' { $dev -match 'proxy-read-timeout: "120"' -and $dev -match 'proxy-send-timeout: "120"' }
Check 'local compatibility: link-only nginx log override and explicit trusted headers' {
    (Doc $dev 'Ingress' 'push-auth-links') -match 'enable-access-log: "false"' -and
    (Doc $dev 'Ingress' 'push-auth') -notmatch 'enable-access-log' -and
    (Doc $dev 'Deployment' 'push-auth') -match 'name: PUSH_AUTH_CLIENT_IP_HEADER\s+value: "x-real-ip"' -and
    (Doc $dev 'Deployment' 'push-gateway') -match 'name: PUSH_CLIENT_IP_HEADER\s+value: "x-real-ip"'
}

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

Check 'portable chart interface matrix' {
    & $python.Source (Join-Path $PSScriptRoot 'test-portability.py') | Out-Host
    $LASTEXITCODE -eq 0
}

# ---- Entra ID datastores (Plan 12B) ----
$entraArgs = @(
    '--set', 'postgres.enabled=false',
    '--set', 'postgres.external.host=pg.example.net',
    '--set', 'postgres.external.users.messageSvc=push-message-svc-id',
    '--set', 'postgres.external.users.auth=push-auth-id',
    '--set', 'postgres.external.users.worker=push-worker-id',
    '--set', 'redis.enabled=false',
    '--set', 'redis.external.url=rediss://cache.example.net:10000',
    '--set', 'azure.workloadIdentity.clientIds.auth=00000000-0000-0000-0000-00000000000a',
    '--set', 'azure.workloadIdentity.clientIds.messageSvc=00000000-0000-0000-0000-00000000000b',
    '--set', 'azure.workloadIdentity.clientIds.gateway=00000000-0000-0000-0000-00000000000c',
    '--set', 'azure.workloadIdentity.clientIds.worker=00000000-0000-0000-0000-00000000000d'
)
Check 'entra: a complete external configuration renders' { (Render $entraArgs) -eq 0 }
Check 'entra: an external PostgreSQL needs a host and every user' {
    (Render ($entraArgs + @('--set', 'postgres.external.host='))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'postgres.external.users.worker='))) -ne 0
}
Check 'entra: an Entra Redis needs a rediss:// URL without credentials' {
    (Render ($entraArgs + @('--set', 'redis.external.url=redis://cache:6379'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'redis.external.url=rediss://:pw@cache:10000'))) -ne 0
}
Check 'entra: every identity that reaches an Entra datastore needs a client id' {
    (Render ($entraArgs + @('--set', 'azure.workloadIdentity.clientIds.gateway='))) -ne 0
}
Check 'entra: the bundled datastores ignore the external settings' {
    $bundled = helm template push $chart --set postgres.external.auth=azure-entra --set redis.external.auth=azure-entra | Out-String
    (Doc $bundled 'Deployment' 'push-auth') -match 'key: auth-database-url' -and $bundled -notmatch 'PUSH_POSTGRES_AUTH'
}
Check 'entra: an external password-mode datastore needs a consumer-managed Secret' {
    (Render @('--set', 'postgres.enabled=false', '--set', 'postgres.external.host=pg',
        '--set', 'postgres.external.auth=password')) -ne 0 -and
    (Render @('--set', 'postgres.enabled=false', '--set', 'postgres.external.host=pg',
        '--set', 'postgres.external.auth=password', '--set', 'secrets.existingSecret=creds')) -eq 0
}
Check 'entra: chart injection needs a tenant; unknown modes are refused' {
    (Render ($entraArgs + @('--set', 'azure.workloadIdentity.injection=chart'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'azure.workloadIdentity.injection=chart',
            '--set', 'azure.workloadIdentity.tenantId=11111111-2222-3333-4444-555555555555'))) -eq 0 -and
        (Render ($entraArgs + @('--set', 'azure.workloadIdentity.injection=magic'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'postgres.external.auth=kerberos'))) -ne 0
}
Check 'entra: the migration Job needs an Entra PostgreSQL, its user and its client id' {
    (Render @('--set', 'migrations.job.enabled=true')) -ne 0 -and
        (Render ($entraArgs + @('--set', 'migrations.job.enabled=true'))) -ne 0 -and
        (Render ($entraArgs + @('--set', 'migrations.job.enabled=true',
            '--set', 'postgres.external.users.migrate=push-migrator-id',
            '--set', 'azure.workloadIdentity.clientIds.migrate=00000000-0000-0000-0000-00000000000e'))) -eq 0
}
Check 'entra: role names are Entra principal names' {
    (Render ($entraArgs + @('--set', 'postgres.external.users.auth=bad"name'))) -ne 0
}
Check 'entra: migration egress policy is a persistent pre-install/pre-upgrade hook before the Job' {
    $rendered = helm template push $chart @entraArgs --set migrations.job.enabled=true `
        --set postgres.external.users.migrate=push-migrator-id `
        --set azure.workloadIdentity.clientIds.migrate=00000000-0000-0000-0000-00000000000e `
        --set networkPolicy.egress.enabled=true `
        --set 'networkPolicy.egress.postgresCidrs={10.20.0.4/32}' `
        --set 'networkPolicy.egress.httpsCidrs={20.0.0.0/8}' | Out-String
    $validate = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
policy = next(d for d in docs if d["kind"] == "NetworkPolicy" and d["metadata"]["name"] == "push-migrate")
job = next(d for d in docs if d["kind"] == "Job" and d["metadata"]["name"] == "push-migrate")
a = policy["metadata"].get("annotations", {})
assert a.get("helm.sh/hook") == "pre-install,pre-upgrade", "migration policy must precede ordinary release resources"
assert int(a["helm.sh/hook-weight"]) < int(job["metadata"]["annotations"]["helm.sh/hook-weight"])
assert a["helm.sh/hook-delete-policy"] == "before-hook-creation", "policy must persist while Job runs"
assert policy["spec"]["podSelector"]["matchLabels"].items() <= job["spec"]["template"]["metadata"]["labels"].items()
ports = {p["port"] for r in policy["spec"]["egress"] for p in r["ports"]}
assert ports == {53, 443, 5432}
for d in docs:
    if d["kind"] == "NetworkPolicy" and d is not policy:
        assert "helm.sh/hook" not in d["metadata"].get("annotations", {}), "ordinary policies must remain ordinary"
'@
    $rendered | & $python.Source -c $validate
    $LASTEXITCODE -eq 0
}
# The default render with the Secret's generated values masked: they are random per render.
function Normalized([string] $rendered) {
    ($rendered -replace "`r`n", "`n" -split "(?m)^---\s*$" | ForEach-Object {
        if ($_ -match '(?m)^kind: Secret\s*$') { $_ -replace '(?m)^(  [a-z0-9-]+): ".*"$', '$1: "<generated>"' } else { $_ }
    }) -join '---'
}
Check 'default: the render is unchanged by Plan 12B' {
    # The baseline is saved before the work starts; absent on CI, where this passes trivially.
    $baseline = Join-Path ([IO.Path]::GetTempPath()) 'push-default-before.yaml'
    (-not (Test-Path $baseline)) -or ((Normalized (Get-Content $baseline -Raw)) -eq (Normalized $default))
}
$entra = helm template push $chart @entraArgs | Out-String
Check 'entra: no bundled datastore, and no datastore credential in any Secret or ConfigMap' {
    $noCredentials = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
names = {(d["kind"], d["metadata"]["name"]) for d in docs}
assert ("StatefulSet", "push-postgres") not in names and ("Deployment", "push-redis") not in names
assert ("Service", "push-postgres") not in names and ("Service", "push-redis") not in names
assert ("ConfigMap", "push-postgres-init") not in names
for d in docs:
    if d["kind"] in ("Secret", "ConfigMap"):
        data = {**(d.get("data") or {}), **(d.get("stringData") or {})}
        bad = [k for k in data if "database" in k or "redis" in k or "db-password" in k or k == "postgres-password"]
        assert not bad, (d["metadata"]["name"], bad)
'@
    $entra | & $python.Source -c $noCredentials; $LASTEXITCODE -eq 0
}
Check 'entra: only the four application policies exist' {
    ($entra -split "(?m)^---\s*$" | Where-Object { $_ -match '(?m)^kind: NetworkPolicy\s*$' }).Count -eq 4 -and
        -not (Doc $entra 'NetworkPolicy' 'push-postgres') -and -not (Doc $entra 'NetworkPolicy' 'push-redis')
}
Check 'entra: each service gets exactly its datastore variables and no datastore secret' {
    $envCheck = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
def env(name):
    d = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == name)
    c = d["spec"]["template"]["spec"]["containers"][0]
    return {e["name"]: e.get("value", e.get("valueFrom")) for e in c["env"]}
pg = {"PUSH_POSTGRES_AUTH": "azure-entra", "PUSH_DATABASE_HOST": "pg.example.net", "PUSH_DATABASE_PORT": "5432", "PUSH_RUN_MIGRATIONS": "true"}
redis = {"PUSH_REDIS_AUTH": "azure-entra", "PUSH_REDIS_URL": "rediss://cache.example.net:10000"}
cases = {
    "push-auth": {**pg, **redis, "PUSH_DATABASE_NAME": "push_auth", "PUSH_DATABASE_USER": "push-auth-id", "PUSH_AUTH_WORKER_ROLE": "push-worker-id"},
    "push-message-svc": {**pg, **redis, "PUSH_DATABASE_NAME": "push_messaging", "PUSH_DATABASE_USER": "push-message-svc-id"},
    "push-worker": {**pg, "PUSH_DATABASE_NAME": "push_worker", "PUSH_DATABASE_USER": "push-worker-id", "PUSH_AUTH_DATABASE_NAME": "push_auth"},
    "push-gateway": redis,
}
for name, expected in cases.items():
    actual = env(name)
    for key, value in expected.items():
        assert actual.get(key) == value, (name, key, actual.get(key))
    assert "PUSH_DATABASE_URL" not in actual and "PUSH_AUTH_DATABASE_URL" not in actual, name
    refs = [v["secretKeyRef"]["key"] for v in actual.values() if isinstance(v, dict) and "secretKeyRef" in v]
    assert not [k for k in refs if "database" in k or "redis" in k], (name, refs)
assert "PUSH_REDIS_AUTH" not in env("push-worker") and "PUSH_POSTGRES_AUTH" not in env("push-gateway")
'@
    $entra | & $python.Source -c $envCheck; $LASTEXITCODE -eq 0
}
Check 'entra webhook: one ServiceAccount per workload with its client id; pods opt in' {
    $sa = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
ids = {"push-auth": "a", "push-message-svc": "b", "push-gateway": "c", "push-worker": "d"}
for name, suffix in ids.items():
    account = next(d for d in docs if d["kind"] == "ServiceAccount" and d["metadata"]["name"] == name)
    assert account["metadata"]["annotations"]["azure.workload.identity/client-id"].endswith(suffix), name
    assert account["automountServiceAccountToken"] is False
    pod = next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == name)["spec"]["template"]
    assert pod["metadata"]["labels"]["azure.workload.identity/use"] == "true", name
    assert pod["spec"]["serviceAccountName"] == name and pod["spec"]["automountServiceAccountToken"] is False
    assert not [e for e in pod["spec"]["containers"][0]["env"] if e["name"].startswith("AZURE_")], name
    assert "volumes" not in pod["spec"] and "volumeMounts" not in pod["spec"]["containers"][0], name
'@
    $entra | & $python.Source -c $sa; $LASTEXITCODE -eq 0
}
Check 'entra chart injection: the chart projects the token, sets AZURE_* and mounts the CAs' {
    $chartInjection = helm template push $chart @entraArgs `
        --set azure.workloadIdentity.injection=chart `
        --set azure.workloadIdentity.tenantId=11111111-2222-3333-4444-555555555555 `
        --set azure.workloadIdentity.authorityCaConfigMap=authority-ca `
        --set postgres.external.caConfigMap=pg-ca --set redis.external.caConfigMap=redis-ca | Out-String
    $projection = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
def pod(name):
    return next(d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] == name)["spec"]["template"]
auth = pod("push-auth")
assert "azure.workload.identity/use" not in auth["metadata"]["labels"]
volumes = {v["name"]: v for v in auth["spec"]["volumes"]}
source = volumes["azure-identity-token"]["projected"]["sources"][0]["serviceAccountToken"]
assert source == {"audience": "api://AzureADTokenExchange", "expirationSeconds": 3600, "path": "azure-identity-token"}
assert volumes["postgres-ca"]["configMap"]["name"] == "pg-ca" and volumes["redis-ca"]["configMap"]["name"] == "redis-ca"
assert volumes["authority-ca"]["configMap"]["name"] == "authority-ca"
container = auth["spec"]["containers"][0]
env = {e["name"]: e.get("value") for e in container["env"]}
assert env["AZURE_CLIENT_ID"].endswith("a") and env["AZURE_TENANT_ID"].startswith("1111")
assert env["AZURE_FEDERATED_TOKEN_FILE"] == "/var/run/secrets/azure/tokens/azure-identity-token"
assert env["AZURE_AUTHORITY_HOST"] == "https://login.microsoftonline.com/"
assert env["SSL_CERT_FILE"] == "/etc/push/authority-ca/ca.crt"
assert env["PUSH_DATABASE_CA_FILE"] == "/etc/push/postgres-ca/ca.crt"
assert env["PUSH_REDIS_CA_FILE"] == "/etc/push/redis-ca/ca.crt"
mounts = {m["mountPath"]: m for m in container["volumeMounts"]}
for path in ["/var/run/secrets/azure/tokens", "/etc/push/postgres-ca", "/etc/push/redis-ca", "/etc/push/authority-ca"]:
    assert mounts[path]["readOnly"] is True, path
# Each workload mounts only the CA bundles of the datastores it reaches.
gateway = {v["name"] for v in pod("push-gateway")["spec"]["volumes"]}
worker = {v["name"] for v in pod("push-worker")["spec"]["volumes"]}
assert gateway == {"azure-identity-token", "authority-ca", "redis-ca"}, gateway
assert worker == {"azure-identity-token", "authority-ca", "postgres-ca"}, worker
'@
    $chartInjection | & $python.Source -c $projection; $LASTEXITCODE -eq 0
}
Check 'default: no ServiceAccount and no identity wiring' {
    $default -notmatch '(?m)^kind: ServiceAccount' -and $default -notmatch 'azure.workload.identity' -and
        $default -notmatch 'serviceAccountName'
}
$jobArgs = $entraArgs + @('--set', 'migrations.job.enabled=true',
    '--set', 'postgres.external.users.migrate=push-migrator-id',
    '--set', 'azure.workloadIdentity.clientIds.migrate=00000000-0000-0000-0000-00000000000e')
Check 'entra: the migration hook Job migrates all three databases as its own identity' {
    $rendered = helm template push $chart @jobArgs | Out-String
    $job = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
job = next(d for d in docs if d["kind"] == "Job" and d["metadata"]["name"] == "push-migrate")
account = next(d for d in docs if d["kind"] == "ServiceAccount" and d["metadata"]["name"] == "push-migrate")
hooks = lambda d: d["metadata"]["annotations"]
assert hooks(job)["helm.sh/hook"] == "pre-install,pre-upgrade"
assert hooks(account)["helm.sh/hook"] == "pre-install,pre-upgrade"
assert int(hooks(account)["helm.sh/hook-weight"]) < int(hooks(job)["helm.sh/hook-weight"])
assert account["metadata"]["annotations"]["azure.workload.identity/client-id"].endswith("e")
assert account["automountServiceAccountToken"] is False
template = job["spec"]["template"]
assert template["metadata"]["labels"]["azure.workload.identity/use"] == "true"
pod = template["spec"]
assert pod["serviceAccountName"] == "push-migrate" and pod["restartPolicy"] == "Never"
assert pod["automountServiceAccountToken"] is False
steps = pod["initContainers"] + pod["containers"]
assert [c["name"] for c in steps] == ["message-svc", "auth", "worker"]
assert [c["image"] for c in steps] == ["push-messenger/message-svc:dev", "push-messenger/auth:dev", "push-messenger/worker:dev"]
expected = {"message-svc": ("push_messaging", "push-message-svc-id"), "auth": ("push_auth", "push-auth-id"), "worker": ("push_worker", "push-worker-id")}
for c in steps:
    env = {e["name"]: e.get("value") for e in c["env"]}
    assert c["args"] == ["migrate"], c["name"]
    assert env["PUSH_POSTGRES_AUTH"] == "azure-entra" and env["PUSH_DATABASE_HOST"] == "pg.example.net"
    assert (env["PUSH_DATABASE_NAME"], env["PUSH_DATABASE_RUNTIME_ROLE"]) == expected[c["name"]]
    assert env["PUSH_DATABASE_USER"] == "push-migrator-id"
    assert not [e for e in c["env"] if "valueFrom" in e], c["name"]
    assert c["securityContext"]["readOnlyRootFilesystem"] is True
    assert c["securityContext"]["runAsNonRoot"] is True
assert {e["name"]: e.get("value") for e in steps[1]["env"]}["PUSH_AUTH_WORKER_ROLE"] == "push-worker-id"
services = [d for d in docs if d["kind"] == "Deployment" and d["metadata"]["name"] in ("push-auth", "push-message-svc", "push-worker")]
assert len(services) == 3
for d in services:
    env = {e["name"]: e.get("value") for e in d["spec"]["template"]["spec"]["containers"][0]["env"]}
    assert env["PUSH_RUN_MIGRATIONS"] == "false", d["metadata"]["name"]
'@
    $rendered | & $python.Source -c $job; $LASTEXITCODE -eq 0
}
Check 'default and plain Entra: no migration Job' {
    $default -notmatch '(?m)^kind: Job' -and $entra -notmatch '(?m)^kind: Job' -and $entra -notmatch 'push-migrate'
}
Check 'egress: opt-in rules per component (DNS, release peers, datastores, HTTPS, extra)' {
    $egressArgs = $jobArgs + @('--set', 'networkPolicy.egress.enabled=true',
        '--set', 'networkPolicy.egress.postgresCidrs={10.1.0.4/32}',
        '--set', 'networkPolicy.egress.redisCidrs={10.1.0.5/32}',
        '--set', 'networkPolicy.egress.httpsCidrs={20.190.128.0/18}',
        '--set', 'networkPolicy.egress.extra[0].to[0].ipBlock.cidr=10.9.0.0/16')
    $rendered = helm template push $chart @egressArgs | Out-String
    $egress = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
policies = {d["metadata"]["name"]: d["spec"] for d in docs if d["kind"] == "NetworkPolicy"}
def peers(rule):
    return [p.get("ipBlock", {}).get("cidr") or p["podSelector"]["matchLabels"].get("push-messenger/network-peer") or "dns" for p in rule["to"]]
def ports(rule):
    return sorted((p["protocol"], p["port"]) for p in rule.get("ports", []))
extra = {"to": [{"ipBlock": {"cidr": "10.9.0.0/16"}}]}
expected = {
    "push-auth": {("message-svc", 50053), ("10.1.0.4/32", 5432), ("10.1.0.5/32", 10000), ("20.190.128.0/18", 443)},
    "push-message-svc": {("10.1.0.4/32", 5432), ("10.1.0.5/32", 10000), ("20.190.128.0/18", 443)},
    "push-gateway": {("auth", 8080), ("message-svc", 50051), ("10.1.0.5/32", 10000), ("20.190.128.0/18", 443)},
    "push-worker": {("message-svc", 50052), ("10.1.0.4/32", 5432), ("20.190.128.0/18", 443)},
    "push-migrate": {("10.1.0.4/32", 5432), ("20.190.128.0/18", 443)},
}
assert set(policies) == set(expected), sorted(policies)
for name, wanted in expected.items():
    spec = policies[name]
    assert spec["policyTypes"] == ["Ingress", "Egress"], name
    rules = spec["egress"]
    assert rules[-1] == extra, name
    rules = rules[:-1]
    dns = [r for r in rules if ports(r) == [("TCP", 53), ("UDP", 53)]]
    assert len(dns) == 1, name
    assert dns[0]["to"] == [{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "kube-system"}},
                             "podSelector": {"matchLabels": {"k8s-app": "kube-dns"}}}], name
    got = {(peer, port) for r in rules if r not in dns for peer in peers(r) for (_, port) in ports(r)}
    assert got == wanted, (name, got)
assert policies["push-migrate"]["ingress"] == []
assert policies["push-migrate"]["podSelector"]["matchLabels"]["app.kubernetes.io/name"] == "migrate"
'@
    $rendered | & $python.Source -c $egress; $LASTEXITCODE -eq 0
}
Check 'egress: with bundled datastores the rules name their pods; the datastores get no egress' {
    $bundled = helm template push $chart --set networkPolicy.egress.enabled=true | Out-String
    $policy = Doc $bundled 'NetworkPolicy' 'push-auth'
    $policy -match 'Egress' -and $policy -match 'network-peer: postgres' -and $policy -match 'network-peer: redis' -and
        (Doc $bundled 'NetworkPolicy' 'push-postgres') -match '(?s)policyTypes:\s+- Ingress\s+- Egress.*egress: \[\]' -and
        (Doc $bundled 'NetworkPolicy' 'push-redis') -match '(?s)policyTypes:\s+- Ingress\s+- Egress.*egress: \[\]'
}
Check 'egress: CIDRs, DNS peers and extra rules are validated' {
    (Render @('--set', 'networkPolicy.egress.enabled=true', '--set', 'networkPolicy.egress.httpsCidrs={login.microsoftonline.com}')) -ne 0 -and
        (Render @('--set', 'networkPolicy.egress.enabled=true', '--set', 'networkPolicy.egress.dns[0].podSelector.k8s-app=kube-dns')) -ne 0 -and
        (Render @('--set', 'networkPolicy.egress.enabled=true', '--set', 'networkPolicy.egress.extra[0]=everything')) -ne 0 -and
        (Render @('--set', 'networkPolicy.egress.enabled=true', '--set', 'networkPolicy.egress.httpsCidrs={0.0.0.0/0}')) -eq 0
}

# ---- Entra ID sign-in (Plan 13B) ----
$signInArgs = @(
    '--set', 'auth.signIn.providers=entra',
    '--set', 'auth.signIn.entra.clientIds={11111111-0000-4000-8000-000000000001}',
    '--set', 'auth.signIn.entra.tenants={33333333-0000-4000-8000-000000000003}'
)
Check 'sign-in: a complete Entra-only configuration renders' { (Render $signInArgs) -eq 0 }
Check 'sign-in: both providers render, in either order' {
    (Render ($signInArgs + @('--set', 'auth.signIn.providers=password\,entra'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.providers=entra\, password'))) -eq 0
}
Check 'sign-in: unknown providers and incomplete Entra settings are refused' {
    (Render @('--set', 'auth.signIn.providers=ldap')) -ne 0 -and
        (Render @('--set', 'auth.signIn.providers=entra\,entra')) -ne 0 -and
        (Render @('--set', 'auth.signIn.providers=entra')) -ne 0 -and
        (Render $signInArgs[0..3]) -ne 0 -and
        (Render ($signInArgs[0..1] + $signInArgs[4..5])) -ne 0
}
Check 'sign-in: tenants are one keyword or tenant ids, never a mixture' {
    $ok = foreach ($tenants in '{common}', '{Organizations}', '{consumers}',
        '{33333333-0000-4000-8000-000000000003,44444444-0000-4000-8000-000000000004}') {
        Render ($signInArgs + @('--set', "auth.signIn.entra.tenants=$tenants"))
    }
    $bad = foreach ($tenants in '{common,33333333-0000-4000-8000-000000000003}', '{organizations,consumers}',
        '{common,common}', '{33333333-0000-4000-8000-000000000003,33333333-0000-4000-8000-000000000003}',
        '{33333333-0000-4000-8000-000000000003,33333333-0000-4000-8000-00000000000A,33333333-0000-4000-8000-00000000000a}',
        '{contoso.onmicrosoft.com}', '{00000000-0000-0000-0000-000000000000}', '{""}') {
        Render ($signInArgs + @('--set', "auth.signIn.entra.tenants=$tenants"))
    }
    $many = '{' + ((1..21 | ForEach-Object { '33333333-0000-4000-8000-{0:d12}' -f $_ }) -join ',') + '}'
    $twenty = '{' + ((1..20 | ForEach-Object { '33333333-0000-4000-8000-{0:d12}' -f $_ }) -join ',') + '}'
    (@($ok | Where-Object { $_ -ne 0 }).Count -eq 0) -and (@($bad | Where-Object { $_ -eq 0 }).Count -eq 0) -and
        (Render ($signInArgs + @('--set', "auth.signIn.entra.tenants=$twenty"))) -eq 0 -and
        (Render ($signInArgs + @('--set', "auth.signIn.entra.tenants=$many"))) -ne 0
}
Check 'sign-in: the audience must include the application id; at most four' {
    (Render ($signInArgs + @('--set', 'auth.signIn.entra.clientIds={api://push-api}'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.clientIds={https://push.example.com,11111111-0000-4000-8000-000000000001}'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.clientIds={api://push-api,11111111-0000-4000-8000-000000000001}'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.clientIds={api://a,api://b,api://c,api://d,11111111-0000-4000-8000-000000000001}'))) -ne 0
}
Check 'sign-in: each clientIds entry is one audience: a comma inside one is refused' {
    $comma = helm template push $chart @signInArgs --set 'auth.signIn.entra.clientIds[1]=api://a\,b' 2>&1 | Out-String
    $commaExit = $LASTEXITCODE
    $two = helm template push $chart @signInArgs --set 'auth.signIn.entra.clientIds[1]=api://a' --set 'auth.signIn.entra.clientIds[2]=api://b' 2>&1 | Out-String
    $twoExit = $LASTEXITCODE
    $commaExit -ne 0 -and $comma -match 'one audience per entry' -and $comma -match 'api://a,b' -and
        $twoExit -eq 0 -and $two -match 'PUSH_ENTRA_CLIENT_ID' -and
        $two -match '11111111-0000-4000-8000-000000000001,api://a,api://b'
}
Check 'sign-in: authority, scope, role, client apps, session age and proxy are checked' {
    (Render ($signInArgs + @('--set', 'auth.signIn.entra.authorityHost=http://login.microsoftonline.com/'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.authorityHost=https://login.microsoftonline.com/tenant/'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.authorityHost=https://login.microsoftonline.us'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.requiredScope=two scopes'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.requiredScope=*'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.requiredRole=two roles'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.allowedClientApps={not-a-guid}'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.maxSessionHours=0'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.maxSessionHours=721'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.maxSessionHours=720'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.httpsProxy=proxy:3128'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.entra.httpsProxy=http://proxy.corp.example:3128'))) -eq 0
}
Check 'sign-in: Entra-only refuses the Resend sender and the webhook' {
    (Render ($signInArgs + @('--set', 'worker.email.sender=resend', '--set', 'worker.email.resendApiKey=re_x',
        '--set', 'worker.email.from=a@example.com'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'worker.email.webhook.enabled=true',
            '--set', 'worker.email.webhookSecret=whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw'))) -ne 0
}
Check 'email: the none sender is for Entra-only installations and refuses the webhook' {
    (Render ($signInArgs + @('--set', 'worker.email.sender=none'))) -eq 0 -and
        (Render @('--set', 'worker.email.sender=none')) -ne 0 -and
        (Render ($signInArgs + @('--set', 'auth.signIn.providers=password\,entra', '--set', 'worker.email.sender=none'))) -ne 0 -and
        (Render @('--set', 'worker.email.sender=smtp')) -ne 0
}
Check 'default: the render is unchanged by Plan 13B' {
    # Saved before the work started; absent on CI, where this passes trivially.
    $baseline = Join-Path ([IO.Path]::GetTempPath()) 'push-default-before-13b.yaml'
    (-not (Test-Path $baseline)) -or ((Normalized (Get-Content $baseline -Raw)) -eq (Normalized (helm template push $chart | Out-String)))
}

Check 'sign-in: the default renders no sign-in variable' {
    (helm template push $chart | Out-String) -notmatch 'PUSH_AUTH_PROVIDERS|PUSH_ENTRA_|HTTPS_PROXY|NO_PROXY|entra-ca'
}
Check 'sign-in: Entra-only renders auth-svc''s variables' {
    $auth = Doc (helm template push $chart @signInArgs | Out-String) 'Deployment' 'push-auth'
    $auth -match '(?s)name: PUSH_AUTH_PROVIDERS\s+value: "entra"' -and
        $auth -match '(?s)name: PUSH_ENTRA_CLIENT_ID\s+value: "11111111-0000-4000-8000-000000000001"' -and
        $auth -match '(?s)name: PUSH_ENTRA_ALLOWED_TENANTS\s+value: "33333333-0000-4000-8000-000000000003"' -and
        $auth -match '(?s)name: PUSH_ENTRA_AUTHORITY_HOST\s+value: "https://login.microsoftonline.com/"' -and
        $auth -match '(?s)name: PUSH_ENTRA_REQUIRED_SCOPE\s+value: "access_as_user"' -and
        $auth -match '(?s)name: PUSH_ENTRA_MAX_SESSION_HOURS\s+value: "24"' -and
        $auth -notmatch 'PUSH_ENTRA_REQUIRED_ROLE|PUSH_ENTRA_ALLOWED_CLIENT_APPS|HTTPS_PROXY|SSL_CERT_FILE'
}
Check 'sign-in: a keyword reaches auth-svc alone, lower-cased; tenant ids joined by commas' {
    $auth = Doc (helm template push $chart @signInArgs --set 'auth.signIn.entra.tenants={Organizations}' | Out-String) 'Deployment' 'push-auth'
    $two = Doc (helm template push $chart @signInArgs --set 'auth.signIn.entra.tenants={33333333-0000-4000-8000-000000000003,44444444-0000-4000-8000-000000000004}' | Out-String) 'Deployment' 'push-auth'
    $auth -match '(?s)name: PUSH_ENTRA_ALLOWED_TENANTS\s+value: "organizations"' -and
        $two -match '(?s)name: PUSH_ENTRA_ALLOWED_TENANTS\s+value: "33333333-0000-4000-8000-000000000003,44444444-0000-4000-8000-000000000004"'
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
        @($rendered -split "(?m)^---\s*$" | Where-Object {
            $_ -match '(?m)^kind: (Deployment|StatefulSet|Job|Pod)\s*$' -and $_ -notmatch '(?m)^  name: push-auth\s*$' -and
                $_ -match 'HTTPS_PROXY|entra-ca|PUSH_ENTRA_' }).Count -eq 0
}
Check 'sign-in: password,entra offers both' {
    $auth = Doc (helm template push $chart @signInArgs --set 'auth.signIn.providers=password\,entra' | Out-String) 'Deployment' 'push-auth'
    $auth -match '(?s)name: PUSH_AUTH_PROVIDERS\s+value: "password,entra"'
}
Check 'sign-in: auth-svc trusts one CA bundle when Plan 12B''s authority bundle is set too' {
    $chartInjection = $entraArgs + $signInArgs + @('--set', 'azure.workloadIdentity.injection=chart',
        '--set', 'azure.workloadIdentity.tenantId=11111111-2222-3333-4444-555555555555',
        '--set', 'azure.workloadIdentity.authorityCaConfigMap=corp-ca')
    $auth = Doc (helm template push $chart @chartInjection --set auth.signIn.entra.caConfigMap=corp-ca | Out-String) 'Deployment' 'push-auth'
    (Render ($chartInjection + @('--set', 'auth.signIn.entra.caConfigMap=other-ca'))) -ne 0 -and
        ([regex]::Matches($auth, 'name: SSL_CERT_FILE')).Count -eq 1 -and
        $auth -match '(?s)name: SSL_CERT_FILE\s+value: /etc/push/authority-ca/ca.crt' -and $auth -notmatch 'entra-ca'
}

Check 'sign-in: Entra-only sends no email and routes no link page' {
    $rendered = helm template push $chart @signInArgs | Out-String
    $worker = Doc $rendered 'Deployment' 'push-worker'
    $worker -match '(?s)name: PUSH_EMAIL_SENDER\s+value: "none"' -and
        $worker -match '(?s)name: PUSH_DEV_MAILBOX\s+value: "0"' -and
        $worker -notmatch 'resend-api-key|resend-webhook-secret' -and
        -not (Doc $rendered 'Ingress' 'push-auth-links') -and -not (Doc $rendered 'Ingress' 'push-worker') -and
        (Doc $rendered 'Ingress' 'push-auth') -match 'path: /v1/auth'
}
Check 'sign-in: Entra-only in dev mode keeps the mailbox off and generates no webhook secret' {
    $rendered = helm template push $chart -f $minikube @signInArgs --set worker.email.webhook.enabled=false | Out-String
    (Doc $rendered 'Deployment' 'push-worker') -match '(?s)name: PUSH_DEV_MAILBOX\s+value: "0"' -and
        (Doc $rendered 'Secret' 'push-secrets') -notmatch 'resend-' -and
        (Doc (helm template push $chart -f $minikube | Out-String) 'Secret' 'push-secrets') -match 'resend-webhook-secret:'
}
Check 'sign-in: password,entra keeps the link pages and the sender' {
    $rendered = helm template push $chart @signInArgs --set 'auth.signIn.providers=password\,entra' | Out-String
    (Doc $rendered 'Ingress' 'push-auth-links') -and
        (Doc $rendered 'Deployment' 'push-worker') -match '(?s)name: PUSH_EMAIL_SENDER\s+value: "log"'
}
Check 'sign-in: an Entra-only consumer Secret needs no email key' {
    $rendered = helm template push $chart @signInArgs --set secrets.existingSecret=push-creds | Out-String
    $rendered -notmatch 'key: resend-' -and $rendered -notmatch '(?m)^kind: Secret'
}

Check 'sign-in egress: auth-svc alone reaches the Entra ID ranges, on 443 only' {
    $rendered = helm template push $chart @signInArgs --set networkPolicy.egress.enabled=true `
        --set 'networkPolicy.egress.entraSignInCidrs={20.190.128.0/18,40.126.0.0/18}' | Out-String
    $policies = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
policies = {d["metadata"]["name"]: d["spec"] for d in docs if d["kind"] == "NetworkPolicy"}
def reaches(name):
    return [(p["ipBlock"]["cidr"], port["port"]) for rule in policies[name].get("egress", [])
            for p in rule.get("to", []) if "ipBlock" in p for port in rule.get("ports", [])]
assert sorted(reaches("push-auth")) == [("20.190.128.0/18", 443), ("40.126.0.0/18", 443)], reaches("push-auth")
for other in ["push-gateway", "push-message-svc", "push-worker"]:
    assert reaches(other) == [], (other, reaches(other))
'@
    $rendered | & $python.Source -c $policies; $LASTEXITCODE -eq 0
}
Check 'sign-in egress: Entra sign-in with egress needs a way to Entra ID; the list holds CIDRs' {
    (Render ($signInArgs + @('--set', 'networkPolicy.egress.enabled=true'))) -ne 0 -and
        (Render ($signInArgs + @('--set', 'networkPolicy.egress.enabled=true',
            '--set', 'auth.signIn.entra.httpsProxy=http://proxy.corp.example:3128'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'networkPolicy.egress.enabled=true',
            '--set', 'networkPolicy.egress.httpsCidrs={20.190.128.0/18}'))) -eq 0 -and
        (Render ($signInArgs + @('--set', 'networkPolicy.egress.enabled=true',
            '--set', 'networkPolicy.egress.entraSignInCidrs={login.microsoftonline.com}'))) -ne 0
}
Check 'sign-in egress: without Entra sign-in the list adds nothing' {
    $rendered = helm template push $chart --set networkPolicy.egress.enabled=true `
        --set 'networkPolicy.egress.entraSignInCidrs={20.190.128.0/18}' | Out-String
    $rendered -notmatch '20\.190\.128\.0/18'
}

Check 'admin: off by default' {
    (helm template push $chart | Out-String) -notmatch 'PUSH_AUTH_ADMIN|auth-admin-token|name: admin|8082'
}
$adminPeer = '[{"namespaceSelector":{"kubernetes.io/metadata.name":"ops"},"podSelector":{"app":"admin-shell"}}]'
Check 'admin: the listener, its Secret key, and a NetworkPolicy rule for its peers only' {
    $rendered = helm template push $chart --set auth.admin.enabled=true --set-json "networkPolicy.adminPeers=$adminPeer" | Out-String
    $auth = Doc $rendered 'Deployment' 'push-auth'
    $rules = @'
import sys, yaml
docs = [d for d in yaml.safe_load_all(sys.stdin.read()) if d]
policy = next(d for d in docs if d["kind"] == "NetworkPolicy" and d["metadata"]["name"] == "push-auth")
rules = {rule["ports"][0]["port"]: rule["from"] for rule in policy["spec"]["ingress"]}
assert set(rules) == {8080, 8082}, rules
assert rules[8082] == [{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "ops"}},
                        "podSelector": {"matchLabels": {"app": "admin-shell"}}}], rules[8082]
assert rules[8080] == [{"podSelector": {"matchLabels": {"app.kubernetes.io/instance": "push",
                                                         "push-messenger/network-peer": "gateway"}}}], rules[8080]
for d in docs:
    if d["kind"] == "Ingress":
        for rule in d["spec"]["rules"]:
            for path in rule["http"]["paths"]:
                assert path["backend"]["service"]["port"] == {"name": "http"}, d["metadata"]["name"]
'@
    $rendered | & $python.Source -c $rules
    $LASTEXITCODE -eq 0 -and
        $auth -match '(?s)name: PUSH_AUTH_ADMIN_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: auth-admin-token' -and
        $auth -match '(?s)name: PUSH_AUTH_ADMIN_ADDR\s+value: "0.0.0.0:8082"' -and
        $auth -match '(?s)name: admin\s+containerPort: 8082' -and
        (Doc $rendered 'Service' 'push-auth') -match '(?s)name: admin\s+port: 8082\s+targetPort: admin' -and
        (Doc $rendered 'Secret' 'push-secrets') -match 'auth-admin-token: "[A-Za-z0-9]{48}"' -and
        @($rendered -split "(?m)^---\s*$" | Where-Object {
            $_ -match '(?m)^kind: (Deployment|StatefulSet|Job|Pod)\s*$' -and $_ -notmatch '(?m)^  name: push-auth\s*$' -and
                $_ -match 'auth-admin-token|PUSH_AUTH_ADMIN' }).Count -eq 0
}
Check 'admin: without peers no rule opens the port; peers need both label maps' {
    (Doc (helm template push $chart --set auth.admin.enabled=true | Out-String) 'NetworkPolicy' 'push-auth') -notmatch '8082' -and
        (Doc (helm template push $chart --set-json "networkPolicy.adminPeers=$adminPeer" | Out-String) 'NetworkPolicy' 'push-auth') -notmatch '8082' -and
        (Render @('--set', 'auth.admin.enabled=true', '--set-json', 'networkPolicy.adminPeers=[{"podSelector":{"a":"b"}}]')) -ne 0 -and
        (Render @('--set', 'auth.admin.enabled=true', '--set-json', 'networkPolicy.adminPeers=[{"namespaceSelector":{},"podSelector":{"a":"b"}}]')) -ne 0 -and
        (Render @('--set', 'auth.admin.enabled=yes')) -ne 0
}
Check 'admin: a consumer-managed Secret is referenced, never generated' {
    $rendered = helm template push $chart --set auth.admin.enabled=true --set secrets.existingSecret=push-creds | Out-String
    (Doc $rendered 'Deployment' 'push-auth') -match '(?s)name: push-creds\s+key: auth-admin-token' -and
        $rendered -notmatch '(?m)^kind: Secret'
}

Check 'sign-in: the DEV ONLY smoke test runs in Entra mode with a token endpoint' {
    $rendered = helm template push $chart -f $minikube @signInArgs --set worker.email.webhook.enabled=false `
        --set smoke.entraTokenUrl=http://push-idp.push-idp.svc:8080/dev/token | Out-String
    (Doc $rendered 'Pod' 'push-smoke') -match '(?s)- --entra-token-url\s+- "http://push-idp.push-idp.svc:8080/dev/token"' -and
        (Doc (helm template push $chart -f $minikube | Out-String) 'Pod' 'push-smoke') -notmatch 'entra-token-url' -and
        (helm template push $chart --set smoke.entraTokenUrl=http://x/dev/token | Out-String) -notmatch 'entra-token-url'
}

if ($failures -gt 0) {
    Write-Output "$failures check(s) failed"
    exit 1
}
Write-Output 'all chart checks passed'
