#!/bin/sh
# Plugin launcher: fetch the pinned subagent-mcp release on first use, then exec it.
# Stdout belongs to the MCP protocol; every message here goes to stderr.
set -eu

REPO=Geek0x0/subagent-mcp
HINT="Install the binary manually (see README, Install) and set SUBAGENT_MCP_BIN to its path."

die() {
	echo "subagent-mcp launcher: $*" >&2
	echo "subagent-mcp launcher: $HINT" >&2
	exit 1
}

if [ -n "${SUBAGENT_MCP_BIN:-}" ]; then
	exec "$SUBAGENT_MCP_BIN" "$@"
fi

here=$(cd "$(dirname "$0")" && pwd)
lock="$here/release.lock"
[ -r "$lock" ] || die "missing $lock"

version=$(sed -n '1s/^version  *//p' "$lock")
[ -n "$version" ] || die "$lock: first line must be 'version <x.y.z>'"

case $(uname -s) in
Linux) os=linux ;;
Darwin) os=darwin ;;
*) die "unsupported OS $(uname -s); only Linux and macOS are supported" ;;
esac
case $(uname -m) in
x86_64 | amd64) arch=amd64 ;;
aarch64 | arm64) arch=arm64 ;;
*) die "unsupported architecture $(uname -m)" ;;
esac

data=${CLAUDE_PLUGIN_DATA:-${XDG_CACHE_HOME:-${HOME:-}/.cache}/subagent-mcp}
bindir="$data/bin/$version"
bin="$bindir/subagent-mcp"
if [ -x "$bin" ]; then
	exec "$bin" "$@"
fi

archive="subagent-mcp_${version}_${os}_${arch}.tar.gz"
want=$(awk -v f="$archive" 'NR > 1 && $2 == f { print $1; exit }' "$lock")
[ -n "$want" ] || die "no checksum for $archive in $lock"

base=${SUBAGENT_MCP_RELEASE_BASE:-https://github.com/$REPO/releases/download}
url="$base/v$version/$archive"

if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
	die "neither sha256sum nor shasum found"
fi

mkdir -p "$bindir" || die "cannot create $bindir"
tmp=$(mktemp -d "$bindir/.tmp.XXXXXX") || die "cannot create temp dir in $bindir"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

echo "subagent-mcp launcher: downloading $url" >&2
curl -fsSL --connect-timeout 15 --max-time 300 -o "$tmp/$archive" "$url" >&2 || die "download failed: $url"

got=$(sha256 "$tmp/$archive")
[ "$got" = "$want" ] || die "checksum mismatch for $archive (expected $want, got $got); nothing was installed"

tar -xzf "$tmp/$archive" -C "$tmp" subagent-mcp >&2 || die "cannot extract subagent-mcp from $archive"
chmod 0755 "$tmp/subagent-mcp"
mv -f "$tmp/subagent-mcp" "$bin" || die "cannot install $bin"

trap - EXIT HUP INT TERM
rm -rf "$tmp"
exec "$bin" "$@"
