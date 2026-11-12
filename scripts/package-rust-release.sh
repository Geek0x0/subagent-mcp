#!/usr/bin/env bash
# Package one Rust release target.
#
# Builds the requested target once with the release profile, stages the binary
# together with README.md and config.example.toml, writes the release archive,
# then verifies the archive name, entries, and non-empty size before returning.
# The verification is the packaging test: a wrong entry set or a zero-byte
# archive fails the script instead of producing an uploadable artifact.
#
# Archive naming matches the Go 0.13.x release assets so scripts/release.lock
# keeps working if the Rust binary is published:
#   subagent-mcp_<version>_<os>_<arch>.tar.gz   (Linux, macOS)
#   subagent-mcp_<version>_<os>_<arch>.zip      (Windows, contains subagent-mcp.exe)
#
# This script does not tag, publish, or touch scripts/release.lock or the plugin
# pin. It is used by .github/workflows/rust-release-dry-run.yml (six targets)
# and .github/workflows/rust-ci.yml (host target only).

set -euo pipefail

usage() {
	cat <<'EOF'
Usage: scripts/package-rust-release.sh --version <version> --target <triple> [options]

Options:
  --version <v>     Release version, for example 0.14.0.
  --target <triple> Rust target triple (see below).
  --out <dir>       Output directory (default: dist).
  --cargo <cmd>     Cargo command to build with (default: cargo). Use
                    "cargo xwin" for the Windows arm64 cross-build fallback.
  --no-build        Package an existing target/<triple>/release binary.
  -h, --help        Show this help.

Supported targets:
  x86_64-unknown-linux-gnu     x86_64-unknown-linux-musl
  aarch64-unknown-linux-gnu    aarch64-unknown-linux-musl
  x86_64-apple-darwin          aarch64-apple-darwin
  x86_64-pc-windows-msvc       aarch64-pc-windows-msvc
EOF
}

die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

version=""
target=""
out="dist"
cargo_cmd="cargo"
do_build=true

while [ $# -gt 0 ]; do
	case "$1" in
	--version)
		[ $# -ge 2 ] || die "--version needs a value"
		version=$2
		shift 2
		;;
	--target)
		[ $# -ge 2 ] || die "--target needs a value"
		target=$2
		shift 2
		;;
	--out)
		[ $# -ge 2 ] || die "--out needs a value"
		out=$2
		shift 2
		;;
	--cargo)
		[ $# -ge 2 ] || die "--cargo needs a value"
		cargo_cmd=$2
		shift 2
		;;
	--no-build)
		do_build=false
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		die "unknown argument: $1"
		;;
	esac
done

[ -n "$version" ] || die "--version is required"
[ -n "$target" ] || die "--target is required"

case "$version" in
[0-9]*.[0-9]*.[0-9]*) ;;
*) die "version must look like 0.14.0, got: $version" ;;
esac

# target triple -> release asset os/arch and Windows executable suffix.
case "$target" in
x86_64-unknown-linux-gnu | x86_64-unknown-linux-musl)
	os=linux
	arch=amd64
	exe_suffix=""
	;;
aarch64-unknown-linux-gnu | aarch64-unknown-linux-musl)
	os=linux
	arch=arm64
	exe_suffix=""
	;;
x86_64-apple-darwin)
	os=darwin
	arch=amd64
	exe_suffix=""
	;;
aarch64-apple-darwin)
	os=darwin
	arch=arm64
	exe_suffix=""
	;;
x86_64-pc-windows-msvc)
	os=windows
	arch=amd64
	exe_suffix=".exe"
	;;
aarch64-pc-windows-msvc)
	os=windows
	arch=arm64
	exe_suffix=".exe"
	;;
*)
	die "unsupported target: $target (see --help)"
	;;
esac

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
cd "$repo_root"

mkdir -p "$out"
out=$(cd -- "$out" && pwd)

binary="target/$target/release/subagent-mcp$exe_suffix"

if $do_build; then
	printf 'building %s (release)\n' "$target"
	# Intentional word splitting: --cargo "cargo xwin" is two argv entries.
	# shellcheck disable=SC2206
	cargo_argv=($cargo_cmd)
	"${cargo_argv[@]}" build --release --locked --target "$target"
else
	printf 'skipping build, packaging existing %s\n' "$binary"
fi

[ -f "$binary" ] || die "missing release binary: $binary"
[ -s "$binary" ] || die "release binary is empty: $binary"

stage=$(mktemp -d "${TMPDIR:-/tmp}/subagent-mcp-package.XXXXXX")
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT

cp "$binary" "$stage/subagent-mcp$exe_suffix"
cp README.md config.example.toml "$stage/"

name="subagent-mcp_${version}_${os}_${arch}"
case "$os" in
windows) archive="$out/$name.zip" ;;
*) archive="$out/$name.tar.gz" ;;
esac

# winpath converts a POSIX path to a Windows path when running under Git Bash.
winpath() {
	if command -v cygpath >/dev/null 2>&1; then
		cygpath -w "$1"
	else
		printf '%s' "$1"
	fi
}

create_zip() {
	local archive_abs=$1
	shift
	local entries=("$@")

	if command -v zip >/dev/null 2>&1; then
		(cd "$stage" && zip -q -X "$archive_abs" "${entries[@]}")
		return
	fi

	if command -v powershell.exe >/dev/null 2>&1; then
		local items=""
		local entry
		for entry in "${entries[@]}"; do
			items="${items:+$items,}'$(winpath "$stage/$entry")'"
		done
		powershell.exe -NoProfile -NonInteractive -Command \
			"Compress-Archive -LiteralPath $items -DestinationPath '$(winpath "$archive_abs")' -Force" ||
			die "Compress-Archive failed for $archive_abs"
		return
	fi

	if command -v python3 >/dev/null 2>&1; then
		python3 - "$archive_abs" "$stage" "${entries[@]}" <<'PY'
import sys, zipfile
archive, stage, entries = sys.argv[1], sys.argv[2], sys.argv[3:]
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zf:
    for entry in entries:
        zf.write(stage + "/" + entry, entry)
PY
		return
	fi

	die "no zip implementation found (need zip, powershell.exe, or python3)"
}

list_zip() {
	local archive_abs=$1

	if command -v unzip >/dev/null 2>&1; then
		unzip -Z1 "$archive_abs"
		return
	fi

	if command -v powershell.exe >/dev/null 2>&1; then
		powershell.exe -NoProfile -NonInteractive -Command \
			"Add-Type -AssemblyName System.IO.Compression.FileSystem; [IO.Compression.ZipFile]::OpenRead('$(winpath "$archive_abs")').Entries | ForEach-Object { \$_.FullName }" ||
			die "could not list $archive_abs"
		return
	fi

	if command -v python3 >/dev/null 2>&1; then
		python3 - "$archive_abs" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1]) as zf:
    for entry in zf.namelist():
        print(entry)
PY
		return
	fi

	die "no zip lister found (need unzip, powershell.exe, or python3)"
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "no sha256 tool found (need sha256sum or shasum)"
	fi
}

case "$os" in
windows)
	create_zip "$archive" "subagent-mcp.exe" "README.md" "config.example.toml"
	entries=$(list_zip "$archive")
	;;
*)
	tar -czf "$archive" -C "$stage" "subagent-mcp" "README.md" "config.example.toml"
	entries=$(tar -tzf "$archive" | sed 's#^\./##')
	;;
esac

expected=$(printf '%s\n' "subagent-mcp$exe_suffix" "README.md" "config.example.toml" | LC_ALL=C sort)
actual=$(printf '%s\n' "$entries" | sed '/^$/d' | LC_ALL=C sort)

[ -s "$archive" ] || die "archive is empty: $archive"
[ "$actual" = "$expected" ] || die "archive contents mismatch for $archive
expected:
$expected
actual:
$actual"

printf 'archive: %s\n' "$archive"
printf 'size: %s bytes\n' "$(wc -c <"$archive" | tr -d ' ')"
printf 'sha256: %s\n' "$(sha256_of "$archive")"
printf 'entries:\n'
printf '%s\n' "$actual" | sed 's/^/  /'
