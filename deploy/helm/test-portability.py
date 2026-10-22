"""Portable chart interface checks: consumer-managed Secrets, explicit email/webhook
references, controller-neutral Ingress and immutable image references.

Renders without a kubeconfig, parses complete resources and never prints rendered
output (a chart-managed render contains credentials). Synthetic values only.

    python deploy/helm/test-portability.py [--case secrets|email|ingress|images]
"""

import argparse
import json
import os
from pathlib import Path
import subprocess

import yaml

CHART = Path(__file__).resolve().parent / "push-messenger"
parser = argparse.ArgumentParser()
parser.add_argument("--case", choices=["secrets", "mounted", "email", "ingress", "images"])
args = parser.parse_args()


def render(values, error=None):
    directory = CHART.parent / (".chart-fixture-" + str(os.getpid()))
    directory.mkdir(exist_ok=True)
    path = directory / "values.json"
    try:
        path.write_text(json.dumps(values), encoding="utf-8")
        env = dict(os.environ, KUBECONFIG=str(directory / "absent"))
        result = subprocess.run(
            ["helm", "template", "portable", str(CHART), "--namespace",
             "consumer", "-f", str(path)],
            env=env, text=True, capture_output=True, timeout=30, check=False,
        )
    finally:
        path.unlink(missing_ok=True)
        directory.rmdir()
    if error is not None:
        assert result.returncode != 0, "invalid values unexpectedly rendered: " + error
        assert error in result.stderr, "expected named validation error missing: " + error
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


def mounted():
    def values():
        return {
            "secrets": {
                "volume": {"csi": {"driver": "secrets-store.csi.k8s.io",
                                    "readOnly": True,
                                    "volumeAttributes": {"secretProviderClass": "consumer-keys"}}},
                "mountPath": "/consumer/keys",
                "files": {"auth-signing-key": "signing-seed"},
            },
            "postgres": {"enabled": False, "external": {"host": "pg.example.test",
                "users": {"auth": "auth", "messageSvc": "message", "worker": "worker",
                          "migrate": "migrate"}}},
            "redis": {"enabled": False, "external": {"url": "rediss://redis.example.test:10000"}},
            "azure": {"workloadIdentity": {"clientIds": dict.fromkeys(
                ["auth", "messageSvc", "gateway", "worker", "migrate"], "synthetic-client")}},
            "migrations": {"job": {"enabled": True}},
            "networkPolicy": {"egress": {"enabled": True}},
        }

    for optional in (False, True):
        case = values()
        if optional:
            case["auth"] = {"admin": {"enabled": True}}
            case["worker"] = {"email": {"sender": "resend",
                "from": "Push <no-reply@example.test>", "webhook": {"enabled": True}}}
            case["azure"]["workloadIdentity"].update({
                "injection": "chart", "tenantId": "synthetic-tenant",
                "authorityCaConfigMap": "consumer-authority-ca"})
        docs = render(case)
        assert not any(doc["kind"] == "Secret" for doc in docs)
        expected = {
            "auth": {"PUSH_AUTH_SIGNING_KEY_FILE": "signing-seed",
                     "PUSH_AUTH_LINK_KEY_FILE": "auth-link-key",
                     "PUSH_DIRECTORY_TOKEN_FILE": "directory-token"},
            "message-svc": {"PUSH_ACCOUNTS_TOKEN_FILE": "accounts-token",
                            "PUSH_DIRECTORY_TOKEN_FILE": "directory-token",
                            "PUSH_MESSAGING_TOKEN_FILE": "messaging-token"},
            "gateway": {"PUSH_MESSAGING_TOKEN_FILE": "messaging-token"},
            "worker": {"PUSH_ACCOUNTS_TOKEN_FILE": "accounts-token"},
        }
        if optional:
            expected["auth"]["PUSH_AUTH_ADMIN_TOKEN_FILE"] = "auth-admin-token"
            expected["worker"].update({"PUSH_RESEND_API_KEY_FILE": "resend-api-key",
                "PUSH_RESEND_WEBHOOK_SECRET_FILE": "resend-webhook-secret"})
        application_names = {
            "PUSH_AUTH_SIGNING_KEY", "PUSH_AUTH_LINK_KEY", "PUSH_ACCOUNTS_TOKEN",
            "PUSH_DIRECTORY_TOKEN", "PUSH_MESSAGING_TOKEN", "PUSH_AUTH_ADMIN_TOKEN",
            "PUSH_RESEND_API_KEY", "PUSH_RESEND_WEBHOOK_SECRET",
        }
        for component, keys in expected.items():
            pod = document(docs, "Deployment", component)["spec"]["template"]["spec"]
            container = pod["containers"][0]
            env = container["env"]
            assert not any("secretKeyRef" in entry.get("valueFrom", {}) for entry in env)
            actual = {entry["name"]: entry["value"] for entry in env
                      if entry["name"].removesuffix("_FILE") in application_names}
            assert actual == {name: "/consumer/keys/" + filename
                              for name, filename in keys.items()}, component
            assert not any(entry["name"] in application_names for entry in env)
            assert {"name": "application-keys", "mountPath": "/consumer/keys",
                    "readOnly": True} in container["volumeMounts"]
            volume = next(v for v in pod["volumes"] if v["name"] == "application-keys")
            assert volume == {"name": "application-keys", **case["secrets"]["volume"]}
        job = document(docs, "Job", "migrate")["spec"]["template"]["spec"]
        for container in job.get("initContainers", []) + job["containers"]:
            assert not any(e["name"].removesuffix("_FILE") in application_names
                           for e in container.get("env", []))
            assert not any(m["name"] == "application-keys"
                           for m in container.get("volumeMounts", []))
        assert not any(v["name"] == "application-keys" for v in job.get("volumes", []))
        policy = document(docs, "NetworkPolicy", "migrate-egress")
        assert policy["metadata"]["annotations"]["helm.sh/hook"] == "pre-install,pre-upgrade"
    for override, error in [
        ({"existingSecret": "consumer"}, "secrets.existingSecret"),
        ({"volume": "csi"}, "secrets.volume"),
        ({"volume": {"name": "bad", "csi": {}}}, "secrets.volume"),
        ({"volume": {"csi": {}}}, "secrets.volume"),
        ({"volume": {"csi": {"driver": ""}}}, "secrets.volume"),
        ({"volume": {"secret": {}}}, "secrets.volume"),
        ({"volume": {"configMap": {}}}, "secrets.volume"),
        ({"volume": {"projected": {"sources": []}}}, "secrets.volume"),
        ({"volume": {"persistentVolumeClaim": {}}}, "secrets.volume"),
        ({"volume": {"csi": {"driver": "secrets-store.csi.k8s.io",
                            "volumeAttributes": {"secretProviderClass": False}}}}, "secrets.volume"),
        ({"mountPath": "../keys"}, "secrets.mountPath"),
        ({"mountPath": "/keys/../secret"}, "secrets.mountPath"),
        ({"mountPath": "/"}, "secrets.mountPath"),
        ({"files": {"auth-signing-key": "../seed"}}, "secrets.files"),
        ({"files": {"auth-link-key": ""}}, "secrets.files"),
        ({"files": {"unknown-key": "key"}}, "secrets.files"),
        ({"files": "key"}, "secrets.files"),
        ({"volume": {"csi": {"driver": "secrets-store.csi.k8s.io"}}}, "secretProviderClass"),
    ]:
        case = values()
        case["secrets"].update(override)
        render(case, error)
    # Bundled datastore credentials remain Secret-backed; only the DEV password
    # smoke hook needs the application signing key. Operator smoke does not.
    case = {"secrets": values()["secrets"],
            "devMode": {"enabled": True, "acknowledge": "insecure-local-only"},
            "ingress": {"host": "push.test", "scheme": "http"}}
    docs = render(case)
    generated = document(docs, "Secret", "secrets")["stringData"]
    assert not any(key in generated for key in [
        "auth-signing-key", "auth-link-key", "accounts-token", "directory-token",
        "messaging-token", "auth-admin-token", "resend-api-key", "resend-webhook-secret"])
    smoke = document(docs, "Pod", "smoke")["spec"]
    assert smoke["containers"][0]["env"] == [
        {"name": "PUSH_SMOKE_SIGNING_KEY_FILE", "value": "/consumer/keys/signing-seed"}]
    assert any(m["name"] == "application-keys" and m["readOnly"]
               for m in smoke["containers"][0]["volumeMounts"])
    case["smoke"] = {"entraAccountsSecret": "operator-accounts"}
    smoke = document(render(case), "Pod", "smoke")["spec"]
    assert not smoke["containers"][0].get("env")
    assert not any(m["name"] == "application-keys"
                   for m in smoke["containers"][0]["volumeMounts"])
    for section, override, error in [
        ("auth", {"signingKey": "synthetic-inline"}, "auth.signingKey"),
        ("worker", {"email": {"resendApiKey": "synthetic-inline"}}, "worker.email.resendApiKey"),
    ]:
        case = values()
        case[section] = override
        render(case, error)
    for filename, enabled, error in [
        ("auth-admin-token", False, None),
        ("auth-admin-token", True, "secrets.files.auth-admin-token"),
        ("resend-api-key", False, None),
        ("resend-api-key", True, "secrets.files.resend-api-key"),
        ("resend-webhook-secret", False, None),
        ("resend-webhook-secret", True, "secrets.files.resend-webhook-secret"),
    ]:
        case = values()
        case["secrets"]["files"][filename] = ""
        if enabled and filename == "auth-admin-token":
            case["auth"] = {"admin": {"enabled": True}}
        elif enabled:
            case["worker"] = {"email": {"sender": "resend",
                "from": "Push <no-reply@example.test>",
                "webhook": {"enabled": filename == "resend-webhook-secret"}}}
        render(case, error)


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


tests = {"secrets": secrets, "mounted": mounted, "email": email, "ingress": ingress, "images": images}
for name, test in tests.items():
    if args.case is None or args.case == name:
        test()
        print("ok   portable " + name)
