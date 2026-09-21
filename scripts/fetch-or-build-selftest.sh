#!/bin/sh
set -eu
root="$(CDPATH= cd "$(dirname "$0")/.." && pwd)"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' 0
mkdir "$test_dir/bin"
cp "${1:-$root/scripts/fetch-or-build.sh}" "$test_dir/fetch-or-build.sh"
for tool in sed head rm; do
  ln -s "$(command -v "$tool")" "$test_dir/bin/$tool"
done
cat > "$test_dir/bin/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" > "$CARGO_LOG"
exit 0
EOF
cat > "$test_dir/bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) printf '%s\n' "$TEST_UNAME_SYSTEM" ;;
  -m) printf '%s\n' x86_64 ;;
esac
EOF
chmod +x "$test_dir/bin/cargo" "$test_dir/bin/uname"
cd "$test_dir"
failed=0
for scenario in no-curl unknown-platform empty-version no-sha256; do
  mkdir -p sentinel
  printf 'keep me\n' > sentinel/keep
  printf '[package]\nname = "herdr-agent-watcher"\nversion = "0.0.0"\n' > Cargo.toml
  system=Linux
  case "$scenario" in
    unknown-platform) system=Unknown ;;
    empty-version) : > Cargo.toml ;;
    no-sha256)
      printf '#!/bin/sh\nexit 1\n' > bin/curl
      chmod +x bin/curl
      ;;
  esac
  : > cargo.log
  TMP="$test_dir/sentinel" TEMP="$test_dir/sentinel" \
    CARGO_LOG="$test_dir/cargo.log" TEST_UNAME_SYSTEM="$system" \
    PATH="$test_dir/bin" /bin/sh ./fetch-or-build.sh
  [ "$(cat cargo.log)" = 'build --release' ] || { echo 'FAIL: fallback did not call cargo' >&2; exit 1; }
  if [ -f sentinel/keep ]; then
    printf 'PASS: %s preserves inherited TMP/TEMP\n' "$scenario"
  else
    printf 'FAIL: %s deleted inherited TMP/TEMP\n' "$scenario" >&2
    failed=1
  fi
done
exit "$failed"
