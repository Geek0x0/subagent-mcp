#!/usr/bin/env bash
# TLS test fixtures and platform trust-root helpers (Task 28).
#
# The platform tests generate their own ephemeral certificates with `rcgen`
# (tests/tls_platform.rs). This script covers the two things a test binary
# cannot do portably:
#
#   1. generate a reusable CA + leaf for the manual live-smoke runs, and
#   2. install / distrust / remove a root in the platform trust store, plus a
#      best-effort cleanup step for CI (`if: always()`).
#
# Linux has no per-user root store in this crate's trust path: the native bundle
# loaded by rustls-native-certs is the store, so `install-ca` writes a bundle and
# prints the `SSL_CERT_FILE` export to use. `install-ca --deny` is unsupported
# there (it exits non-zero): omit the CA from the bundle instead.
#
# Usage:
#   scripts/tls-test-fixtures.sh generate <dir>
#   scripts/tls-test-fixtures.sh install-ca <ca.pem> [--deny]
#   scripts/tls-test-fixtures.sh remove-ca <ca.pem>
#   scripts/tls-test-fixtures.sh cleanup <common-name>
#   scripts/tls-test-fixtures.sh selftest
#
# Everything here is test-only: certificates are valid for one day and are never
# committed.
set -euo pipefail

# Common name of the temporary root used by tests/tls_platform.rs and
# `generate`. It is not a real CA.
TEST_CA_NAME="subagent-mcp platform trust test CA"

usage() {
	sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//'
	exit "${1:-2}"
}

die() {
	echo "tls-test-fixtures: $*" >&2
	exit 1
}

platform() {
	case "$(uname -s)" in
	Darwin) echo macos ;;
	Linux) echo linux ;;
	MINGW* | MSYS* | CYGWIN*) echo windows ;;
	*) die "unsupported host: $(uname -s)" ;;
	esac
}

need() {
	command -v "$1" >/dev/null 2>&1 || die "required tool not found: $1"
}

cmd_generate() {
	[ $# -eq 1 ] || usage
	local dir=$1
	need openssl
	mkdir -p "$dir"
	openssl req -x509 -newkey rsa:2048 -nodes -keyout "$dir/ca.key" \
		-out "$dir/ca.pem" -days 1 -subj "/CN=$TEST_CA_NAME" >/dev/null 2>&1
	openssl req -newkey rsa:2048 -nodes -keyout "$dir/leaf.key" \
		-out "$dir/leaf.csr" -subj "/CN=127.0.0.1" >/dev/null 2>&1
	printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' \
		>"$dir/leaf.ext"
	openssl x509 -req -in "$dir/leaf.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" \
		-CAcreateserial -out "$dir/leaf.pem" -days 1 -extfile "$dir/leaf.ext" >/dev/null 2>&1
	openssl x509 -in "$dir/ca.pem" -outform der -out "$dir/ca.cer"
	echo "wrote $dir/ca.pem, $dir/ca.key, $dir/leaf.pem, $dir/leaf.key, $dir/ca.cer"
}

cmd_install() {
	[ $# -ge 1 ] && [ $# -le 2 ] || usage
	local pem=$1 trust=trustRoot
	[ -f "$pem" ] || die "no such CA file: $pem"
	[ "${2:-}" = "--deny" ] && trust=deny

	case "$(platform)" in
	linux)
		if [ "$trust" = deny ]; then
			die "Linux has no per-user distrust store in this trust path: omit the CA from the SSL_CERT_FILE bundle instead of marking it denied"
		fi
		local bundle
		bundle="$(cd "$(dirname "$pem")" && pwd)/ca-bundle.pem"
		cat "$pem" >>"$bundle"
		echo "export SSL_CERT_FILE=$bundle"
		;;
	macos)
		need security
		security add-trusted-cert -r "$trust" \
			-k "$HOME/Library/Keychains/login.keychain-db" "$pem"
		;;
	windows)
		need certutil
		need cygpath
		local store=Root
		[ "$trust" = deny ] && store=Disallowed
		certutil -addstore -user "$store" "$(cygpath -w "$pem")" >/dev/null
		echo "installed $(basename "$pem") in CurrentUser\\$store"
		;;
	esac
}

cmd_remove() {
	[ $# -eq 1 ] || usage
	local pem=$1
	[ -f "$pem" ] || die "no such CA file: $pem"

	case "$(platform)" in
	linux)
		local bundle
		bundle="$(cd "$(dirname "$pem")" && pwd)/ca-bundle.pem"
		rm -f "$bundle"
		echo "removed $bundle"
		;;
	macos)
		need security
		security remove-trusted-cert "$pem" || true
		security delete-certificate -c "$TEST_CA_NAME" \
			"$HOME/Library/Keychains/login.keychain-db" || true
		;;
	windows)
		need certutil
		local store
		for store in Root Disallowed; do
			certutil -delstore -user "$store" "$TEST_CA_NAME" >/dev/null 2>&1 || true
		done
		;;
	esac
}

cmd_cleanup() {
	[ $# -eq 1 ] || usage
	local name=$1

	case "$(platform)" in
	linux)
		echo "nothing to clean up on Linux (bundle files live in the test's temp dir)"
		;;
	macos)
		need security
		security delete-certificate -c "$name" \
			"$HOME/Library/Keychains/login.keychain-db" >/dev/null 2>&1 || true
		echo "cleaned $name from the login keychain (if present)"
		;;
	windows)
		need certutil
		local store
		for store in Root Disallowed; do
			certutil -delstore -user "$store" "$name" >/dev/null 2>&1 || true
		done
		echo "cleaned $name from CurrentUser Root/Disallowed (if present)"
		;;
	esac
}

cmd_selftest() {
	echo "platform: $(platform)"
	echo "openssl: $(command -v openssl || echo missing)"
	case "$(platform)" in
	macos) echo "security: $(command -v security || echo missing)" ;;
	windows) echo "certutil: $(command -v certutil || echo missing)" ;;
	linux) echo "native roots: SSL_CERT_FILE bundle path" ;;
	esac
}

[ $# -ge 1 ] || usage
command=$1
shift
case "$command" in
generate) cmd_generate "$@" ;;
install-ca) cmd_install "$@" ;;
remove-ca) cmd_remove "$@" ;;
cleanup) cmd_cleanup "$@" ;;
selftest) cmd_selftest "$@" ;;
-h | --help | help) usage 0 ;;
*) usage ;;
esac
