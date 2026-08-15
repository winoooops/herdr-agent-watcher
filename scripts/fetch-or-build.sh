#!/bin/sh
# The plugin's [[build]] step. Its only contract is to leave a working binary at
# target/release/herdr-agent-watcher.
#
# Herdr runs build commands during `plugin install`, and nothing says a build
# command has to compile — so this fetches a published release asset when one
# matches the platform, and compiles when anything at all goes wrong. Every
# failure path falls back to `cargo build --release`, which is what this step
# used to do unconditionally: the worst case is the old behaviour, never a
# failed install.
set -eu

REPO="winoooops/herdr-agent-watcher"
BIN="herdr-agent-watcher"
OUT="target/release/$BIN"

# `exec` so the fallback replaces this shell: cargo's exit status becomes ours,
# and a failed compile fails the install exactly as it did before.
build() {
    [ -n "${TMP:-}" ] && rm -rf "$TMP"
    echo "herdr-agent-watcher: building from source ($*)" >&2
    exec cargo build --release
}

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[ -n "$version" ] || build "no version in Cargo.toml"

# Only the four targets release.yml publishes. Anything else compiles.
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)   target="aarch64-apple-darwin" ;;
    Darwin-x86_64)  target="x86_64-apple-darwin" ;;
    Linux-x86_64)   target="x86_64-unknown-linux-musl" ;;
    Linux-aarch64)  target="aarch64-unknown-linux-musl" ;;
    *)              build "no published asset for $(uname -s)-$(uname -m)" ;;
esac

command -v curl >/dev/null 2>&1 || build "curl not found"
if command -v sha256sum >/dev/null 2>&1; then
    sha() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
    sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    build "no sha256 tool"
fi

asset="$BIN-$version-$target"
# Overridable so the download and verification paths can be exercised against
# a local file:// tree in tests. Anyone able to set this already controls the
# environment the build runs in.
base="${HERDR_AGENT_WATCHER_RELEASE_BASE:-https://github.com/$REPO/releases/download/v$version}"
TMP=$(mktemp -d) || build "cannot create a temp dir"

curl -fsSL --retry 2 --max-time 120 "$base/$asset" -o "$TMP/$BIN" \
    || build "no asset $asset in release v$version"
curl -fsSL --retry 2 --max-time 30 "$base/SHA256SUMS" -o "$TMP/SHA256SUMS" \
    || build "release v$version has no SHA256SUMS"

want=$(grep "  $asset\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)
[ -n "$want" ] || build "$asset is not listed in SHA256SUMS"

got=$(sha "$TMP/$BIN")
# A mismatch is not a reason to guess. Compile instead, and say why.
[ "$want" = "$got" ] || build "checksum mismatch for $asset: expected $want, got $got"

mkdir -p target/release
chmod +x "$TMP/$BIN"
mv "$TMP/$BIN" "$OUT"
rm -rf "$TMP"
echo "herdr-agent-watcher: installed prebuilt $asset" >&2
