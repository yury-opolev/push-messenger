# Security review, 2026-10-17

A codebase audit against the OWASP Top 10:2025 found 13 issues: 2 P1, 4 P2, 5 P3 and
2 P4. Category A05 (Injection) had no findings. Every finding is resolved below. The
review was static: it does not replace a penetration test of a consumer's deployment.

| ID | Priority | Category | Finding | Resolution |
|---|---|---|---|---|
| F1 | P1 | A09 | auth-svc logged no security events | A middleware logs one structured event under the `security` target for each security-relevant answer: sign-in success and failure, refused refresh and access tokens, rate limits, invalid links, password resets, device registration, limits and revocations. Refresh-token reuse is logged too. Events carry the route, status, error code, client IP and user and device ids; never emails, passwords or tokens. |
| F2 | P1 | A03 | No dependency scanning or update automation | `deny.toml` and a `supply-chain` CI job run `cargo deny check` and `npm audit --omit=dev`; Dependabot covers cargo, npm, GitHub Actions, the Dockerfile and pip. The first run found rustls 0.23.45 (RUSTSEC-2026-0346 to -0349) and the dev-only time 0.3.45 (RUSTSEC-2026-0009); both are updated. SBOMs belong to the consumer's image publication. |
| F3 | P2 | A07 | A password reset left earlier access tokens working at auth-svc's API | auth-svc records the reset second and refuses access tokens issued no later than that second, as message-svc already did. |
| F4 | P2 | A06 | No per-user write quota | `Send`, `Edit` and `Delete` share a per-user quota across devices: 120 a minute and 3,000 an hour. Over it, the write gets a retryable `RATE_LIMITED` with its `client_msg_id`. The quota fails open while Redis is down, like `Identify`. |
| F5 | P2 | A09 | The gateway did not log refused connections | Refused `Identify` attempts, connection and write rate limits, and `ReAuth` tokens naming another device are logged as `security` events with the client IP. |
| F6 | P2 | A08/A04 | The gateway's key set and the internal hops are plaintext | The gateway can fetch the key set over `https://` (rustls; system roots or `SSL_CERT_FILE`), set through `gateway.authUrl`. The deployment guide states that internal traffic is unencrypted and should be protected by a service mesh with mTLS or an encrypting CNI, which the consumer chooses. |
| F7 | P3 | A10 | A revoked or reset session stayed open when its kick was lost | Kicks report failure, so the relayed revocation or reset fails and the worker retries it. The gateway closes the connection (4003 or 4001) whenever message-svc refuses a call with `UNKNOWN_DEVICE` or `PASSWORD_CHANGED`, and completes the WebSocket close handshake so the client receives the code. |
| F8 | P3 | A09 | message-svc did not log refused internal tokens | Refused internal API calls are logged with the API name and peer address, never the token. |
| F9 | P3 | A03 | Images pulled by mutable tag | The Dockerfile's base images and the chart's default PostgreSQL and Redis images are pinned by digest; a chart check keeps them pinned. |
| F10 | P3 | A03 | CI actions and PyYAML not pinned | Every action is pinned to a commit SHA, and the chart tests install PyYAML with `--require-hashes`. |
| F11 | P3 | A02 | The bundled PostgreSQL container was not hardened | No privilege escalation, a read-only root filesystem, and only the capabilities its entrypoint needs; the server runs with none. Verified on an upgraded volume and a fresh install. |
| F12 | P4 | A04 | The SDK accepted plaintext URLs to any host | `Client::open` requires `https://` and `wss://` except to this machine, unless `allow_insecure_transport` (`allowInsecureTransport`) is set for a development cluster. |
| F13 | P4 | A03 | testcontainers pulled tokio-tar 0.3.1 (RUSTSEC-2025-0111) | testcontainers-modules 0.15 uses astral-tokio-tar. It needs Rust 1.88 for the tests only, so the MSRV job now builds the workspace on 1.85 and the tests run on stable. |

Behaviour changes for consumers:

- SDK apps that connect over `http://` or `ws://` to anything but this machine must set
  `allowInsecureTransport` or move to TLS.
- Clients that write more than 120 times a minute per user get `RATE_LIMITED`; the SDK
  retries such sends with the same id after `retry_after_s`.
- Upgrading restarts the bundled PostgreSQL and Redis once (new image references and
  security context).
- Logs gain `security` events; route them to the consumer's alerting.
