#!/usr/bin/env bash
# Task 30 cutover checks. Run before deleting the Go tree (expected to fail),
# then after the cutover and after pushing go-v0.13.1 (expected to pass).
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

fail=0
check() {
  local label=$1
  shift
  if "$@"; then
    printf 'ok   %s\n' "$label"
  else
    printf 'FAIL %s\n' "$label" >&2
    fail=1
  fi
}

version=$(awk -F'"' '/^version = "/ { print $2; exit }' Cargo.toml)
check "Cargo.toml version is 0.14.0" test "$version" = "0.14.0"
check "VERSION is 0.14.0" grep -q '^pub const VERSION: &str = "0.14.0";$' src/lib.rs

check "Go source tree is deleted" test ! -e cmd
check "Go internal tree is deleted" test ! -e internal
check "go.mod is deleted" test ! -e go.mod
check "go.sum is deleted" test ! -e go.sum

check "release.yml does not build Go" bash -c '! grep -Eq "go build|CGO_ENABLED|cmd/subagent-mcp/main\\.go" .github/workflows/release.yml'
check "release.yml calls the reusable runtime gates" grep -q 'uses: \./\.github/workflows/rust-ci\.yml' .github/workflows/release.yml
check "publish job depends on runtime-gates" grep -Eq 'needs:.*runtime-gates' .github/workflows/release.yml
check "publish uses gh release create" grep -q 'gh release create' .github/workflows/release.yml
check "publish is not clobbering" bash -c '! grep -q -- "--clobber" .github/workflows/release.yml'
check "rust-ci exposes workflow_call" grep -q 'workflow_call:' .github/workflows/rust-ci.yml

check "release.lock still pins 0.13.1" grep -q '^version 0\.13\.1$' scripts/release.lock
check "plugin still pins 0.13.1" grep -q '"version": "0\.13\.1"' .claude-plugin/plugin.json
check "differential baseline is still 0.13.1" grep -q '^version = "0\.13\.1"$' tests/differential/baseline.lock
check "update-release-lock has no Go toolchain dependency" bash -c '! grep -Eq "go build|go install|GOTOOLCHAIN|go run" scripts/update-release-lock.sh'

check "README has no go install command" bash -c '! grep -Eq "go install (github\\.com|\\./cmd)" README.md'
check "setup command has no go install command" bash -c '! grep -Eq "go install (github\\.com|\\./cmd)" commands/setup.md'

expected_tag=fe563a12ad30046c0d5be27fdddf9f18d39e8e47
local_tag=$(git rev-parse go-v0.13.1 2>/dev/null || true)
check "local go-v0.13.1 points at the pinned Go commit" test "$local_tag" = "$expected_tag"
remote_tag=$(git ls-remote --tags origin refs/tags/go-v0.13.1 2>/dev/null | awk '{print $1}' | head -1)
check "origin go-v0.13.1 points at the pinned Go commit" test "$remote_tag" = "$expected_tag"

if [ "$fail" -ne 0 ]; then
  echo "cutover checks failed" >&2
  exit 1
fi
echo "cutover checks passed"
