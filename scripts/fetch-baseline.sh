#!/bin/sh
# Fetch the pinned Go baseline used by the differential harness.
#
# Downloads tests/differential/baseline.lock's release artifact into
# target/reference/, verifies its SHA-256, and extracts `subagent-mcp` there.
# The script is idempotent: a verified archive is reused, and the binary is
# re-extracted from it on every run so a stale binary cannot survive.
#
# Usage: scripts/fetch-baseline.sh [--force]
#   --force  re-download the archive even when the existing copy verifies
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
manifest="$root/tests/differential/baseline.lock"
force=0

for arg in "$@"; do
	case "$arg" in
	--force) force=1 ;;
	-h | --help)
		sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
		exit 0
		;;
	*)
		echo "fetch-baseline: unknown argument: $arg" >&2
		exit 2
		;;
	esac
done

[ -r "$manifest" ] || {
	echo "fetch-baseline: missing $manifest" >&2
	exit 1
}

manifest_value() {
	awk -v key="$1" '
		$0 ~ "^" key " *= *\"" {
			sub("^" key " *= *\"", "", $0)
			sub(/".*$/, "", $0)
			print
			exit
		}
	' "$manifest"
}

version=$(manifest_value version)
repo=$(manifest_value repo)
artifact=$(manifest_value artifact)
want=$(manifest_value sha256)
binary_name=$(manifest_value binary)
binary_name=${binary_name:-subagent-mcp}

for name in version repo artifact binary_name; do
	eval "value=\$$name"
	[ -n "$value" ] || {
		echo "fetch-baseline: $manifest is missing $name" >&2
		exit 1
	}
done
[ -n "$want" ] || {
	echo "fetch-baseline: $manifest is missing sha256" >&2
	exit 1
}

if command -v sha256sum >/dev/null 2>&1; then
	sha256_file() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256_file() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
	echo "fetch-baseline: neither sha256sum nor shasum found" >&2
	exit 1
fi

dest=${SUBAGENT_MCP_REFERENCE_DIR:-$root/target/reference}
archive="$dest/$artifact"
binary="$dest/$binary_name"
base=${SUBAGENT_MCP_RELEASE_BASE:-https://github.com/$repo/releases/download}
url="${base%/}/v$version/$artifact"

mkdir -p "$dest"
tmp_archive="$dest/.fetch.$$"
tmp_dir="$dest/.extract.$$"
trap 'rm -rf "$tmp_archive" "$tmp_dir"' EXIT HUP INT TERM

if [ "$force" -eq 1 ]; then
	rm -f "$archive"
fi

if [ -f "$archive" ] && [ "$(sha256_file "$archive")" = "$want" ]; then
	echo "fetch-baseline: using verified $archive"
else
	echo "fetch-baseline: downloading $url"
	curl -fsSL --retry 3 --connect-timeout 15 --max-time 300 \
		-o "$tmp_archive" "$url" || {
		echo "fetch-baseline: download failed: $url" >&2
		exit 1
	}
	got=$(sha256_file "$tmp_archive")
	[ "$got" = "$want" ] || {
		echo "fetch-baseline: checksum mismatch for $artifact" >&2
		echo "fetch-baseline: expected $want, got $got" >&2
		exit 1
	}
	mv -f "$tmp_archive" "$archive"
fi

mkdir -p "$tmp_dir"
tar -xzf "$archive" -C "$tmp_dir" "$binary_name" || {
	echo "fetch-baseline: cannot extract $binary_name from $archive" >&2
	exit 1
}
chmod 0755 "$tmp_dir/$binary_name"
mv -f "$tmp_dir/$binary_name" "$binary"
echo "fetch-baseline: ready $binary"
