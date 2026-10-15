#!/usr/bin/env bash
# Replaces the vendored SQLite3 Multiple Ciphers (crates/push-sdk/vendor/sqlite3mc) with
# the amalgamation of an upstream release, after verifying it. The twin of
# update-sqlite3mc.ps1; see there for what is verified and why.
#
#   scripts/update-sqlite3mc.sh 2.5.1
#
# Needs gh (signed in), curl, unzip, sha256sum (or shasum), and optionally cosign (or
# COSIGN=/path/to/cosign). Downloads go to target/sqlite3mc-update.
set -euo pipefail

version="${1:?usage: $0 <version, e.g. 2.5.1>}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "version must look like 2.5.1" >&2; exit 1; }

repo='utelle/SQLite3MultipleCiphers'
# The trust anchor: Sigstore's public-good root (through gh and cosign) plus this exact
# certificate identity of upstream's release workflow.
signer='https://github.com/utelle/SQLite3MultipleCiphers/.github/workflows/build-reusable.yml@refs/heads/main'
issuer='https://token.actions.githubusercontent.com'

root="$(cd "$(dirname "$0")/.." && pwd)"
vendor="$root/crates/push-sdk/vendor/sqlite3mc"
work="$root/target/sqlite3mc-update"
tag="v$version"
files=(sqlite3mc_amalgamation.c sqlite3mc_amalgamation.h)

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
fail() { echo "error: $*" >&2; exit 1; }

command -v gh >/dev/null || fail 'gh (GitHub CLI) is required'
rm -rf "$work"
mkdir -p "$work"

echo "Downloading $repo $tag"
sums="sqlite3mc-$version-SHA256SUMS"
gh release download "$tag" -R "$repo" -D "$work" -p "$sums" -p "$sums.pem" -p "$sums.sig" \
    -p "sqlite3mc-$version-sqlite-*-amalgamation.zip" >/dev/null
shopt -s nullglob
zips=("$work"/sqlite3mc-"$version"-sqlite-*-amalgamation.zip)
[[ ${#zips[@]} -eq 1 ]] || fail "expected one amalgamation zip in release $tag"
zip="${zips[0]}"
zip_name="$(basename "$zip")"
sqlite_version="$(sed -nE "s/^sqlite3mc-${version//./\\.}-sqlite-([0-9]+\.[0-9]+\.[0-9]+)-amalgamation\.zip$/\1/p" <<<"$zip_name")"
[[ -n "$sqlite_version" ]] || fail "unexpected asset name $zip_name"

echo 'Verifying the GitHub Artifact Attestations'
source_commit=''
for file in "$zip" "$work/$sums"; do
    commit="$(gh attestation verify "$file" -R "$repo" --cert-identity "$signer" \
        --cert-oidc-issuer "$issuer" --deny-self-hosted-runners --format json \
        --jq '.[0].verificationResult.signature.certificate.sourceRepositoryDigest')" \
        || fail "attestation of $file failed"
    [[ -n "$commit" ]] || fail "the attestation of $file names no source commit"
    [[ -z "$source_commit" || "$source_commit" == "$commit" ]] \
        || fail 'the attestations name different source commits'
    source_commit="$commit"
done

cosign="${COSIGN:-$(command -v cosign || true)}"
if [[ -n "$cosign" ]]; then
    echo 'Verifying the cosign signature of SHA256SUMS'
    "$cosign" verify-blob --certificate "$work/$sums.pem" --signature "$work/$sums.sig" \
        --certificate-identity "$signer" --certificate-oidc-issuer "$issuer" "$work/$sums" \
        >/dev/null 2>&1 || fail 'cosign verify-blob failed'
else
    echo 'warning: cosign not found: the cosign signature of SHA256SUMS was not checked (the attestations were)' >&2
fi

zip_sha="$(sha256 "$zip")"
listed="$(awk -v name="$zip_name" '{ f = $2; sub(/^\*/, "", f) } f == name { print $1 }' "$work/$sums")"
[[ "$listed" == "$zip_sha" ]] || fail "$zip_name has SHA-256 $zip_sha, which SHA256SUMS does not list"
version_file="$vendor/VERSION"
if [[ -f "$version_file" ]]; then
    pinned_release="$(sed -n 's/^release: *//p' "$version_file")"
    pinned_sha="$(sed -n 's/^asset-sha256: *//p' "$version_file")"
    if [[ "$pinned_release" == "$tag" && "$pinned_sha" != "$zip_sha" ]]; then
        fail "$zip_name has SHA-256 $zip_sha, but VERSION pins $pinned_sha for $tag"
    fi
fi

extract="$work/amalgamation"
unzip -q "$zip" -d "$extract"
for file in "${files[@]}"; do
    [[ -f "$extract/$file" ]] || fail "$file is missing from $zip_name"
done
header="$extract/sqlite3mc_amalgamation.h"
grep -qE "^#define SQLITE3MC_VERSION_STRING[[:space:]]+\"SQLite3 Multiple Ciphers ${version//./\\.}\"" "$header" \
    || fail "the header does not declare SQLite3 Multiple Ciphers $version"
grep -qE "^#define SQLITE_VERSION[[:space:]]+\"${sqlite_version//./\\.}\"" "$header" \
    || fail "the header does not declare SQLite $sqlite_version"

echo "Fetching LICENSE at $source_commit"
curl -fsSL --proto '=https' -o "$work/LICENSE" "https://raw.githubusercontent.com/$repo/$source_commit/LICENSE"
tag_commit="$(gh api "repos/$repo/commits/$tag" --jq '.sha')"
if [[ "$tag_commit" != "$source_commit" ]]; then
    echo "warning: tag $tag is commit $tag_commit, but the release was built from $source_commit" >&2
fi

mkdir -p "$vendor"
for file in "${files[@]}"; do
    cp "$extract/$file" "$vendor/$file"
done
cp "$work/LICENSE" "$vendor/LICENSE"
touch "$vendor/${files[0]}" "$vendor/${files[1]}" "$vendor/LICENSE"

{
    echo "# SQLite3 Multiple Ciphers, vendored for push-sdk's build.rs."
    echo '# Written by scripts/update-sqlite3mc.ps1 (or .sh); do not edit by hand.'
    echo '# build.rs fails the build unless every file: line below has exactly its SHA-256.'
    echo "release: $tag"
    echo "sqlite3mc: $version"
    echo "sqlite: $sqlite_version"
    echo "asset: $zip_name"
    echo "asset-sha256: $zip_sha"
    echo "sha256sums-sha256: $(sha256 "$work/$sums")"
    echo "source-commit: $source_commit"
    echo "signer: $signer"
    echo "oidc-issuer: $issuer"
    echo "retrieved: $(date -u +%Y-%m-%d)"
    for file in "${files[@]}" LICENSE; do
        echo "file: $(sha256 "$vendor/$file")  $file"
    done
} >"$version_file"

rm -rf "$work"
echo "Vendored SQLite3 Multiple Ciphers $version (SQLite $sqlite_version), built from $source_commit."
echo 'Next: cargo test -p push-sdk --locked, then review and commit crates/push-sdk/vendor/sqlite3mc.'
