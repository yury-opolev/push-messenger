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
$withWebhookControllers = helm template push $chart --set-json "networkPolicy.ingressControllerPeers=$peerJson" --set worker.email.webhookSecret=whsec_test | Out-String
Check 'network: controller peers reach the worker only with webhook exposure' {
    PolicyMatrix $withWebhookControllers 'push' $controllerPeers $true
}
$withoutIngress = helm template push $chart --set-json "networkPolicy.ingressControllerPeers=$peerJson" --set worker.email.webhookSecret=whsec_test --set ingress.enabled=false | Out-String
Check 'network: disabling webhook Ingress removes controller access to the worker' {
    PolicyMatrix $withoutIngress 'push' $controllerPeers
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
        $gateway -match 'name: PUSH_CLIENT_IP_HEADER\s+value: x-real-ip' -and
        $default -notmatch 'PUSH_DEV_AUTH'
}
Check 'default: groups are refused; one switch serves them' {
    $on = helm template push $chart --set gateway.groups=true | Out-String
    (Doc $default 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "0"' -and
        (Doc $on 'Deployment' 'push-gateway') -match 'name: PUSH_GROUPS\s+value: "1"'
}
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' -and $default -notmatch 'PUSH_SMOKE_SIGNING_KEY' }
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

if ($failures -gt 0) {
    Write-Output "$failures check(s) failed"
    exit 1
}
Write-Output 'all chart checks passed'
