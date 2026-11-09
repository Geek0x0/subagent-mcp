#!/bin/sh
# Maintainer tool: pin scripts/release.lock to a published release.
# Usage: scripts/update-release-lock.sh <version>   (e.g. 0.12.3)
set -eu

[ $# -eq 1 ] || { echo "usage: $0 <version>" >&2; exit 2; }
version=${1#v}
lock="$(cd "$(dirname "$0")" && pwd)/release.lock"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

curl -fsSL --connect-timeout 15 --max-time 60 -o "$tmp" \
	"https://github.com/Geek0x0/subagent-mcp/releases/download/v$version/checksums.txt"
grep -q "subagent-mcp_${version}_linux_amd64.tar.gz" "$tmp" || { echo "checksums.txt does not look like release $version" >&2; exit 1; }

{ echo "version $version"; cat "$tmp"; } > "$lock"
echo "wrote $lock" >&2
