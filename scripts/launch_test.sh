#!/bin/sh
# Offline check of scripts/launch.sh. Usage: sh scripts/launch_test.sh
set -eu

src=$(cd "$(dirname "$0")" && pwd)
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
fail=0
check() { # check <name> <condition exit status>
	if [ "$2" -eq 0 ]; then echo "ok   $1"; else echo "FAIL $1"; fail=1; fi
}

case $(uname -s) in Linux) os=linux ;; Darwin) os=darwin ;; *) echo "unsupported host"; exit 1 ;; esac
case $(uname -m) in x86_64 | amd64) arch=amd64 ;; *) arch=arm64 ;; esac
v=9.9.9
archive="subagent-mcp_${v}_${os}_${arch}.tar.gz"

# fake release + scripts copy + lock
mkdir -p "$t/pkg" "$t/rel/v$v" "$t/scripts" "$t/data" "$t/home"
printf '#!/bin/sh\necho "stand-in $*"\n' > "$t/pkg/subagent-mcp"
chmod +x "$t/pkg/subagent-mcp"
tar -C "$t/pkg" -czf "$t/rel/v$v/$archive" subagent-mcp
cp "$src/launch.sh" "$t/scripts/launch.sh"
sum=$(sha256sum "$t/rel/v$v/$archive" 2>/dev/null || shasum -a 256 "$t/rel/v$v/$archive")
printf 'version %s\n%s  %s\n' "$v" "${sum%% *}" "$archive" > "$t/scripts/release.lock"

run() { # run with isolated env; stdout -> $t/out, stderr -> $t/err
	env -u SUBAGENT_MCP_BIN HOME="$t/home" CLAUDE_PLUGIN_DATA="$t/data" \
		SUBAGENT_MCP_RELEASE_BASE="file://$t/rel" "$@" >"$t/out" 2>"$t/err"
}
bin="$t/data/bin/$v/subagent-mcp"

# (a) first run
rc=0; run sh "$t/scripts/launch.sh" a "b c" || rc=$?
check "a: exit 0" $rc
check "a: stdout is only stand-in output" $([ "$(cat "$t/out")" = "stand-in a b c" ]; echo $?)
check "a: installed 0755" $([ -x "$bin" ]; echo $?)
check "a: no leftover temp dirs" $([ -z "$(find "$t/data" -name '.tmp.*')" ]; echo $?)

# (b) second run without the release
rm -rf "$t/rel"
rc=0; run sh "$t/scripts/launch.sh" again || rc=$?
check "b: cached run ok without release" $rc
check "b: output" $([ "$(cat "$t/out")" = "stand-in again" ]; echo $?)

# (c) tampered archive
rm -rf "$t/data/bin"; mkdir -p "$t/rel/v$v"
printf '#!/bin/sh\necho PWNED\n' > "$t/pkg/subagent-mcp"
tar -C "$t/pkg" -czf "$t/rel/v$v/$archive" subagent-mcp
rc=0; run sh "$t/scripts/launch.sh" x || rc=$?
check "c: non-zero exit" $([ $rc -ne 0 ]; echo $?)
check "c: nothing installed" $([ ! -e "$bin" ]; echo $?)
check "c: nothing executed" $(! grep -q PWNED "$t/out"; echo $?)
check "c: names mismatch and recovery" $(grep -q mismatch "$t/err" && grep -q SUBAGENT_MCP_BIN "$t/err"; echo $?)
check "c: no leftover temp dirs" $([ -z "$(find "$t/data" -name '.tmp.*' 2>/dev/null)" ]; echo $?)

# (d) override: no lock, no network
printf '#!/bin/sh\necho "override $*"\n' > "$t/ovr"; chmod +x "$t/ovr"
rm -f "$t/scripts/release.lock"
rc=0; SUBAGENT_MCP_BIN="$t/ovr" HOME="$t/home" CLAUDE_PLUGIN_DATA="$t/data" SUBAGENT_MCP_RELEASE_BASE=file:///nonexistent \
	sh "$t/scripts/launch.sh" q >"$t/out" 2>"$t/err" || rc=$?
check "d: override exec'd" $([ $rc -eq 0 ] && [ "$(cat "$t/out")" = "override q" ]; echo $?)

# (e) missing lock entry, unsupported platform
printf 'version %s\n%s  other.tar.gz\n' "$v" "$(printf 0 | sha256sum | cut -d' ' -f1)" > "$t/scripts/release.lock"
rc=0; run sh "$t/scripts/launch.sh" || rc=$?
check "e: missing entry fails cleanly" $([ $rc -ne 0 ] && grep -q "no checksum" "$t/err"; echo $?)
mkdir "$t/fakebin"; printf '#!/bin/sh\necho Plan9\n' > "$t/fakebin/uname"; chmod +x "$t/fakebin/uname"
rc=0; PATH="$t/fakebin:$PATH" run sh "$t/scripts/launch.sh" || rc=$?
check "e: unsupported platform fails cleanly" $([ $rc -ne 0 ] && grep -q unsupported "$t/err" && grep -q SUBAGENT_MCP_BIN "$t/err"; echo $?)

[ $fail -eq 0 ] && echo "all passed" || { echo "FAILED"; exit 1; }
