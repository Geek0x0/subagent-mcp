# TLS verifier spike (Task 8)

Status: **Linux verified. macOS and Windows are PENDING NATIVE EVIDENCE.**
The user waived the implementation gate before Task 9; provider transport and
adapters are implemented. Native results remain a **release gate**. This document must not be read as a passed native gate until the
macOS and Windows sections carry real command output.

- Design reference: `docs/superpowers/specs/2026-11-09-rust-rewrite-design.md`
  §5.4 (shared transport) and §7 (security design).
- Implementation: `src/provider/tls.rs`, `examples/tls_probe.rs`.
- Consumer: `DecisionClient::new` (auto selection) already builds its HTTP
  client with `tls::client_config()`. Provider transport also wires
  `provider::transport::shared_client` to the same function.

## 1. Backend selection

| Platform | Verifier | Roots | Source of truth |
|---|---|---|---|
| macOS | `rustls-platform-verifier` → Security.framework | Keychain (system + user) with platform trust/distrust decisions | `tls::PLATFORM_VERIFIER_MACOS_BACKEND` |
| Windows | `rustls-platform-verifier` → CryptoAPI/CNG | Machine + user certificate stores with platform trust/distrust decisions | `tls::PLATFORM_VERIFIER_WINDOWS_BACKEND` |
| Linux | rustls webpki verifier | `rustls-native-certs` (system CA bundle) | `tls::NATIVE_ROOTS_BACKEND` |

Rules enforced by `client_config()`:

- No insecure fallback: if the native bundle yields zero usable roots, client
  construction returns `TlsError` instead of an empty or bundled root store.
- macOS/Windows never use native-roots-only. Extra roots (`--ca-file`,
  `client_config_with_extra_roots`) are **added** to platform trust; they never
  replace or bypass it.
- No `webpki-roots`, no `aws-lc-rs`, no OpenSSL. `reqwest` is built with
  `default-features = false` and `rustls-tls-manual-roots`, and the configured
  `rustls::ClientConfig` is passed with `use_preconfigured_tls`.
- The rustls crypto provider is pinned to `ring`.
- The client config advertises ALPN `h2` then `http/1.1`. reqwest selects
  HTTP/2 from the negotiated ALPN, so this preserves Go's HTTP/2-capable
  transport; without it the preconfigured TLS config would be HTTP/1.1 only.

Dependency invariants. The enforcing check is the manifest unit test
(`provider::tls::tests::manifest_uses_manual_roots_and_ring_without_bundled_roots`),
run by `cargo test --all-targets` on every platform. The literal `cargo tree`
commands exit non-zero precisely because the crates are absent:

```console
$ cargo tree -i webpki-roots
error: package ID specification `webpki-roots` did not match any packages
$ cargo tree -i aws-lc-rs
error: package ID specification `aws-lc-rs` did not match any packages
```

`rustls-platform-verifier` depends on `webpki-root-certs` for its non-native
fallbacks; that is a different crate and is not used on macOS/Windows, where the
OS verifier decides.

## 2. Linux evidence (actual)

Environment:

```console
$ uname -srm
Linux 7.0.0-42-generic x86_64
$ grep PRETTY_NAME /etc/os-release
PRETTY_NAME="Ubuntu 24.04.5 LTS"
$ rustc --version
rustc 1.94.1 (e408947bf 2026-03-25)
$ cargo --version
cargo 1.94.1 (29ea6fb6a 2026-03-24)
$ openssl version
OpenSSL 3.0.13 30 Jan 2024 (Library: OpenSSL 3.0.13 30 Jan 2024)
```

### 2.1 Inline trust tests

```console
$ cargo test provider::tls
running 11 tests
test provider::tls::tests::backend_name_matches_the_platform ... ok
test provider::tls::tests::client_config_builds_with_a_verifier ... ok
test provider::tls::tests::expired_certificate_is_rejected ... ok
test provider::tls::tests::hostname_mismatch_is_rejected ... ok
test provider::tls::tests::intermediate_chain_is_accepted ... ok
test provider::tls::tests::intermediate_chain_without_the_intermediate_is_rejected ... ok
test provider::tls::tests::invalid_extra_root_fails_closed ... ok
test provider::tls::tests::manifest_uses_manual_roots_and_ring_without_bundled_roots ... ok
test provider::tls::tests::native_roots_are_loaded ... ok
test provider::tls::tests::self_signed_certificate_is_rejected ... ok
test provider::tls::tests::temporary_root_certificate_is_accepted ... ok
test result: ok. 11 passed; 0 failed
```

The temporary-root test signs a leaf with a generated CA and trusts only that
CA through `client_config_with_extra_roots`. Only the self-signed test uses
`client_config()` (system roots). The hostname-mismatch test also uses
`client_config_with_extra_roots` (temporary CA), so it isolates the name check
from root trust; it must fail with `NotValidForName`. The expired test uses the
temporary CA with a leaf valid only in 2020 and must fail with
`certificate expired`. The intermediate-chain tests use root -> intermediate ->
leaf: acceptance when the server sends the intermediate, rejection when it does
not, both with only the root injected as an extra root.
`client_config_builds_with_a_verifier` pins the `h2`/`http/1.1` ALPN list, and
`invalid_extra_root_fails_closed` proves a malformed extra root is an error, not
a silently skipped root.

### 2.2 Probe: system roots

```console
$ cargo run --example tls_probe -- --url https://example.com --expect accept
backend: native-roots+webpki
url: https://example.com
expect: accept
decision: accepted (status 200, HTTP/2.0)
EXIT: 0
```

`HTTP/2.0` here confirms the ALPN list reaches reqwest through the
preconfigured TLS config.

### 2.3 Probe: distrusted root (fresh self-signed leaf)

```console
$ cargo run --example tls_probe -- --self-signed --expect reject
backend: native-roots+webpki
url: https://127.0.0.1:33317/
expect: reject
decision: rejected
detail: error sending request for url (https://127.0.0.1:33317/): client error (Connect): invalid peer certificate: UnknownIssuer
EXIT: 0
```

### 2.4 Probe: enterprise CA, extra root

```console
$ cargo run --example tls_probe -- --enterprise-ca --expect accept
backend: native-roots+webpki
url: https://127.0.0.1:43925/
expect: accept
decision: accepted (status 200, HTTP/1.1)
EXIT: 0
```

### 2.5 Probe: enterprise CA installed in the system bundle

A CA and leaf were generated with OpenSSL (`CN=TLS Probe Enterprise CA`,
leaf SAN `IP:127.0.0.1`) and served with `openssl s_server`. The system-bundle
path was exercised through `SSL_CERT_FILE` (honored by `rustls-native-certs`)
instead of modifying the host's root store.

```console
# without the enterprise CA: rejected
$ cargo run --example tls_probe -- --url https://127.0.0.1:9443/ --expect reject
backend: native-roots+webpki
url: https://127.0.0.1:9443/
expect: reject
decision: rejected
detail: error sending request for url (https://127.0.0.1:9443/): client error (Connect): invalid peer certificate: UnknownIssuer
EXIT: 0

# with the enterprise CA on the native-roots path (no --ca-file): accepted
$ SSL_CERT_FILE=/tmp/opencode/tls-ca/ca.pem \
    cargo run --example tls_probe -- --url https://127.0.0.1:9443/ --expect accept
backend: native-roots+webpki
url: https://127.0.0.1:9443/
expect: accept
decision: accepted (status 200, HTTP/1.0)
EXIT: 0

# explicit --ca-file also accepts, and the same server without it is rejected
$ cargo run --example tls_probe -- --url https://127.0.0.1:9443/ \
    --ca-file /tmp/opencode/tls-ca/ca.pem --expect accept
decision: accepted (status 200, HTTP/1.0)
EXIT: 0
```

(`HTTP/1.0` is `openssl s_server -www`, which does not offer HTTP/2.)

### 2.6 Probe: expired certificate

A root-signed leaf with fixed validity 2020-01-01..2020-01-02 (deterministic,
not clock-dependent) must be rejected even though its root is trusted:

```console
$ cargo run --example tls_probe -- --expired --expect reject
backend: native-roots+webpki
url: https://127.0.0.1:37651/
expect: reject
decision: rejected
detail: error sending request for url (https://127.0.0.1:37651/): client error (Connect): invalid peer certificate: certificate expired: verification time 1794425348 (UNIX), but certificate is not valid after 1577923200 (216502148 seconds ago)
EXIT: 0
```

### 2.7 Probe: intermediate chain

Root -> intermediate -> leaf. The server sends leaf + intermediate; only the
root is injected as a trusted extra root, so acceptance depends on the
intermediate:

```console
$ cargo run --example tls_probe -- --intermediate-chain --expect accept
backend: native-roots+webpki
url: https://127.0.0.1:34795/
expect: accept
decision: accepted (status 200, HTTP/1.1)
EXIT: 0
```

### 2.8 Auto-selection client wiring

`DecisionClient` tests prove reqwest actually receives the preconfigured TLS
config:

- `decision::tests::build_http_client_uses_the_configured_tls_roots` — a local
  server whose leaf is signed by a temporary root succeeds only because the
  extra root reaches reqwest through `use_preconfigured_tls`.
- `decision::tests::build_http_client_rejects_an_untrusted_certificate` — the
  production `build_http_client` path (system roots) rejects the same local
  server with a certificate error.

## 3. macOS evidence — PENDING NATIVE EVIDENCE

Not run. This environment is Linux-only. **Do not claim the native release gate passed.**

Exact commands to run on a native macOS host (macOS 12+, Xcode CLT installed):

```console
$ rustup toolchain install 1.94.1 --profile minimal --component rustfmt --component clippy
$ cargo test provider::tls -- --nocapture
$ cargo run --example tls_probe -- --url https://example.com --expect accept
$ cargo run --example tls_probe -- --self-signed --expect reject
$ cargo run --example tls_probe -- --enterprise-ca --expect accept
$ cargo run --example tls_probe -- --expired --expect reject
$ cargo run --example tls_probe -- --intermediate-chain --expect accept
```

Enterprise CA in the System Keychain (admin required):

```console
$ work=$(mktemp -d)
$ openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work/ca.key" \
    -out "$work/ca.pem" -days 1 -subj "/CN=TLS Probe Enterprise CA"
$ openssl req -newkey rsa:2048 -nodes -keyout "$work/leaf.key" \
    -out "$work/leaf.csr" -subj "/CN=127.0.0.1"
$ printf 'subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' \
    > "$work/leaf.ext"
$ openssl x509 -req -in "$work/leaf.csr" -CA "$work/ca.pem" -CAkey "$work/ca.key" \
    -CAcreateserial -out "$work/leaf.pem" -days 1 -extfile "$work/leaf.ext"
$ openssl s_server -accept 127.0.0.1:9443 -cert "$work/leaf.pem" \
    -key "$work/leaf.key" -www -quiet &
$ sudo security add-trusted-cert -d -r trustRoot \
    -k /Library/Keychains/System.keychain "$work/ca.pem"
$ cargo run --example tls_probe -- --url https://127.0.0.1:9443/ --expect accept
$ sudo security add-trusted-cert -d -r deny \
    -k /Library/Keychains/System.keychain "$work/ca.pem"
$ cargo run --example tls_probe -- --url https://127.0.0.1:9443/ --expect reject
$ sudo security delete-certificate -c "TLS Probe Enterprise CA" \
    /Library/Keychains/System.keychain
```

Record: macOS version (`sw_vers`), arch (`uname -m`), every command, and the
full probe output. Expected: system-root accept, self-signed reject, extra-root
accept, expired reject, intermediate-chain accept, Keychain trust accept,
Keychain deny reject. The decisive case is the Keychain one: it proves the
platform verifier honors OS trust **and** distrust, which native-roots-only
cannot do.

## 4. Windows evidence — PENDING NATIVE EVIDENCE

Not run. Exact commands to run on a native Windows host (Windows 10 / Server
2016+, Developer Mode not required; PowerShell as Administrator for the store
steps):

```powershell
rustup toolchain install 1.94.1 --profile minimal --component rustfmt --component clippy
cargo test provider::tls -- --nocapture
cargo run --example tls_probe -- --url https://example.com --expect accept
cargo run --example tls_probe -- --self-signed --expect reject
cargo run --example tls_probe -- --enterprise-ca --expect accept
cargo run --example tls_probe -- --expired --expect reject
cargo run --example tls_probe -- --intermediate-chain --expect accept
```

Enterprise CA in the machine store (Administrator):

```powershell
$work = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "tls-probe")
$openssl = "C:\Program Files\Git\usr\bin\openssl.exe"
& $openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work\ca.key" `
    -out "$work\ca.pem" -days 1 -subj "/CN=TLS Probe Enterprise CA"
& $openssl req -newkey rsa:2048 -nodes -keyout "$work\leaf.key" `
    -out "$work\leaf.csr" -subj "/CN=127.0.0.1"
"subjectAltName=IP:127.0.0.1`nbasicConstraints=CA:FALSE`nextendedKeyUsage=serverAuth" |
    Out-File -Encoding ascii "$work\leaf.ext"
& $openssl x509 -req -in "$work\leaf.csr" -CA "$work\ca.pem" -CAkey "$work\ca.key" `
    -CAcreateserial -out "$work\leaf.pem" -days 1 -extfile "$work\leaf.ext"
& $openssl x509 -in "$work\ca.pem" -outform der -out "$work\ca.cer"
$ca = Import-Certificate -FilePath "$work\ca.cer" -CertStoreLocation Cert:\LocalMachine\Root
$server = Start-Process -FilePath $openssl -PassThru -ArgumentList @(
    "s_server", "-accept", "127.0.0.1:9443", "-cert", "$work\leaf.pem",
    "-key", "$work\leaf.key", "-www", "-quiet")
Start-Sleep -Seconds 2
cargo run --example tls_probe -- --url https://127.0.0.1:9443/ --expect accept
# distrusted root: same CA in the Disallowed store must now be rejected
Import-Certificate -FilePath "$work\ca.cer" -CertStoreLocation Cert:\LocalMachine\Disallowed | Out-Null
cargo run --example tls_probe -- --url https://127.0.0.1:9443/ --expect reject
Stop-Process -Id $server.Id
Remove-Item "Cert:\LocalMachine\Root\$($ca.Thumbprint)"
certutil -delstore Disallowed "TLS Probe Enterprise CA"
```

Record: `[System.Environment]::OSVersion`, `$PSVersionTable`, `uname -m` from
Git Bash or `$env:PROCESSOR_ARCHITECTURE`, every command, and the full probe
output. Expected: system-root accept, self-signed reject, extra-root accept,
expired reject, intermediate-chain accept, machine-store trust accept,
`Disallowed` store reject. The decisive cases are the machine-store trust and
the `Disallowed` distrust: they prove CryptoAPI/CNG policy is honored rather
than a static root list.

## 5. Gate decision

- Linux: verified above. Linux uses system roots + webpki, matching Go's
  `crypto/x509` system-root behavior.
- macOS: **PENDING NATIVE EVIDENCE**. Release remains gated until the commands
  in §3 are run and their output is appended here.
- Windows: **PENDING NATIVE EVIDENCE**. Release remains gated until the commands
  in §4 are run and their output is appended here.
- If either platform verifier cannot reproduce OS trust policy (for example the
  enterprise-CA, distrusted-root, expired-certificate, or intermediate-chain
  case disagrees with the OS), record the OS-native TLS stack fallback decision
  here **before release**. Do not silently fall back to webpki or native roots on
  macOS/Windows.

The probes are automated in `.github/workflows/tls-probe.yml` (manual
`workflow_dispatch`; it uploads each platform's evidence as an artifact). It has
not been pushed or triggered. Once the native jobs are green, paste their
artifacts into §3 and §4 and flip the status line.

## 6. Known limits

- `--ca-file` adds roots to platform trust; it does not install them in the OS
  store. The store cases in §3/§4 are the release gate.
- Expired-certificate and intermediate-chain cases are covered by the inline
  tests and the `--expired` / `--intermediate-chain` probe modes on every
  platform the workflow runs. The native runs are still **PENDING** (§3/§4), so
  they are evidence to collect, not a passed gate.
- The workflow currently runs one architecture per OS (`macos-latest` = arm64,
  `windows-latest` = amd64). Design §8.5 additionally requires macOS amd64 and
  Windows arm64 runtime TLS evidence for the release matrix; those runners must
  supply native evidence before release.
- Error classification in the probe is heuristic (it looks for
  certificate/handshake text in the error chain). Every run prints the full
  error chain, so a misclassification is visible in the evidence.
