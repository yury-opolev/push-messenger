# Coturn brotli CVE (CVE-2026-111497): patched image, image gate, rescans

Urgent security fix (Sev2 on DEV): the chart's coturn image
`coturn/coturn:4.18.0-alpine@sha256:c29135c0…` (Alpine 3.24.1) ships brotli-libs 1.2.0-r1
(required by gnutls); CVE-2026-111497 is fixed in 1.2.1-r0.

## Findings

- Upstream has not rebuilt: `4.18.0-alpine`, `alpine` still resolve to `sha256:c29135c0…`.
  The Debian `4.18.0` image has 82 fixable HIGH/CRITICAL findings (bind9, apt, …): not an
  option (c).
- `apk upgrade` on the pinned base brings brotli-libs 1.2.1-r0 and fixes every other
  fixable finding (bind, mongo-c-driver, zlib, OpenSSL, musl); Trivy 0.76.1: 0 HIGH/CRITICAL.
- Why CI missed it: `deploy/helm/coturn.trivyignore.yaml` accepted CVE-2026-111497 when
  the chart's coturn was added (3d12b7c), reasoning that brotli serves an unused HTTP
  client; brotli is in fact pulled in by gnutls. The scan also ran only on pushes and pull
  requests, and only for coturn: PostgreSQL, Redis and the distroless base of our own
  images had (newer) fixable findings that nothing scanned.

## Plan (option b: derived image)

1. `deploy/coturn/Dockerfile`: `FROM coturn/coturn:4.18.0-alpine@<digest>`, `USER root`,
   `apk upgrade --no-cache` plus a brotli-libs >= 1.2.1-r0 check, `USER nobody:nogroup`;
   OCI labels (source links the GHCR package to the repository).
2. `.github/workflows/coturn-image.yml`: on `coturn-*` tags, main (inputs changed) and
   dispatch: build amd64, check user/brotli, Trivy gate, then push amd64+arm64 with
   provenance and SBOM to `ghcr.io/yury-opolev/push-coturn`. Publish
   `4.18.0-alpine-r1` from the branch by tag and pin its digest in the same PR.
3. CI gate: `deploy/helm/chart-images.sh` lists the chart's third-party images (helm
   template with every component on, pinned by digest); `image-scan` matrix fails on
   fixable HIGH/CRITICAL; `images` scans the images built here. Ignore files per image in
   `deploy/helm/trivyignore/` (only gosu's Go stdlib in PostgreSQL, path-scoped).
4. Move PostgreSQL/Redis pins to current digests; runtime base to distroless cc-debian13.
5. `.github/workflows/image-rescan.yml`: weekly + dispatch, fresh DB, issue on failure.
6. Docs (calls.md §13.4/§13.7, deployment-configuration.md), CHANGELOG, tests' pinned
   references (test-chart*.ps1, Rust tests parsing values.yaml).

## Verification

- Published image: `apk list -I brotli-libs` → 1.2.1-r0; user 65534; entrypoint/cmd as upstream.
- Trivy on the published digest: 0 fixable HIGH/CRITICAL.
- `cargo test -p push-auth-core --test turn` (new image), `test-chart.ps1`, CI chart-cluster
  (kind 1.34–1.37, restricted PSS, coturn Ready, TURN allocation).
- Gate control: a pull request pinning the old upstream digest fails `image-scan`.
