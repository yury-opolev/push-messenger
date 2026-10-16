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
import tempfile

import yaml

CHART = Path(__file__).resolve().parent / "push-messenger"
parser = argparse.ArgumentParser()
parser.add_argument("--case", choices=["secrets", "email", "ingress", "images"])
args = parser.parse_args()


def render(values, error=None):
    with tempfile.TemporaryDirectory(prefix="push-chart-") as directory:
        path = Path(directory) / "values.json"
        path.write_text(json.dumps(values), encoding="utf-8")
        env = dict(os.environ, KUBECONFIG=str(Path(directory) / "absent"))
        result = subprocess.run(
            ["helm", "template", "portable", str(CHART), "--namespace",
             "consumer", "-f", str(path)],
            env=env, text=True, capture_output=True, timeout=30, check=False,
        )
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


tests = {"secrets": secrets, "email": email, "ingress": ingress, "images": images}
for name, test in tests.items():
    if args.case is None or args.case == name:
        test()
        print("ok   portable " + name)
