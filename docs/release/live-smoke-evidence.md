# Platform runtime and TLS trust evidence (Task 28)

Status: **Linux x86_64 verified. Linux arm64, macOS x86_64/arm64, Windows
x86_64, and Windows arm64 are PENDING NATIVE EVIDENCE.**

This file records the per-platform release gate added by Task 28. The Linux
result below is a real local run. Every other row is a checklist to execute on
its native runner; **do not** read a pending row as a passed gate, and do not
substitute a cross-build for a native run.

- Design reference: `docs/superpowers/specs/2026-11-09-rust-rewrite-design.md`
  §5.6/§8.2 (process cleanup), §7 (security design), §8.5 (release matrix).
- Tests: `tests/platform_linux.rs`, `tests/platform_macos.rs`,
  `tests/platform_windows.rs`, `tests/tls_platform.rs`.
- CI jobs: `.github/workflows/rust-ci.yml` → `platform-linux`, `platform-macos`,
  `platform-windows`.
- Fixtures / trust-root helper: `scripts/tls-test-fixtures.sh`.
- TLS verifier spike (Task 8): `docs/release/tls-verifier-spike.md`.

## 1. Test matrix

Every platform file is crate-level `#![cfg(target_os = "...")]`, so
`cargo test --all-targets` compiles all of them on every runner and executes
only the applicable file. `tests/tls_platform.rs` compiles everywhere and gates
its OS-specific setup with `#[cfg(target_os = "...")]`.

| Platform | Runner | Test command | Runtime / cleanup | TLS trust | Status |
|---|---|---|---|---|---|
| Linux x86_64 | `ubuntu-24.04` | `cargo test --test platform_linux --test tls_platform -v` | Landlock helper, `openat2`, process group, WaitDelay | native bundle (`SSL_CERT_FILE`) | **verified locally (see §2)** |
| Linux arm64 | `ubuntu-24.04-arm` | same | same | same | **PENDING NATIVE EVIDENCE** |
| macOS x86_64 | `macos-15-intel` | `cargo test --test platform_macos --test tls_platform -v` + `cargo test --test tls_platform -- --ignored -v` | path sandbox, process group | Security.framework, login keychain | **PENDING NATIVE EVIDENCE** |
| macOS arm64 | `macos-15` | same | same | same | **PENDING NATIVE EVIDENCE** |
| Windows x86_64 | `windows-2022` | `cargo test --test platform_windows --test tls_platform -v` + `cargo test --test tls_platform -- --ignored -v` | Job Object suspended start/assign/resume, path checks, whole-job cleanup | CryptoAPI, CurrentUser stores | **PENDING NATIVE EVIDENCE** |
| Windows arm64 | `windows-11-arm` | same | same | same | **PENDING NATIVE EVIDENCE** |

SIGTERM with stdin open is an **ignored skeleton** on every platform
(`#[ignore = "requires Task 25 serve lifecycle"]`). Task 25 owns `cli::serve`
and the signal handler; until it lands there is no crate code on that path, so
the skeleton is not a gate. Task 25 must replace the shell/`pause` child with
the real `subagent-mcp` server and remove the ignore attribute.

The OS trust-store case (`platform_store_trust_and_distrust_apply`) is also
`#[ignore]`d so the general `verify` matrix reports it as **ignored**, never as
a pass. The dedicated macOS/Windows jobs run it explicitly with `--ignored`
and `SUBAGENT_MCP_PLATFORM_TRUST_TESTS=1`.

What each platform file pins:

- **Linux** (`platform_linux.rs`): the real `subagent-mcp` Landlock helper
  (in-root write allowed, out-of-root denied, malformed argv → exit 126),
  `openat2(RESOLVE_BENEATH)` refusal of absolute and relative symlink escapes,
  post-exit process-group sweep of a backgrounded descendant, and `WaitDelay`
  zero-exit (`ErrWaitDelay` → success) and non-zero precedence (exit 7 wins)
  with an escaped descendant holding the pipes. The `openat2` case starts with
  a positive control (safe write/read and an in-root symlink) so a wholly
  broken `fs()` cannot pass the escape assertions. SIGTERM with stdin open is
  the ignored Task 25 skeleton.
- **macOS** (`platform_macos.rs`): the path sandbox (no kernel enforcement,
  helper fails closed with `landlock unavailable: kernel sandbox requires
  Linux`), policy-level confinement (symlink resolution, `..` rejection,
  outside-root denial), held-directory inode identity, and process-group
  cleanup. SIGTERM with stdin open is the ignored Task 25 skeleton.
- **Windows** (`platform_windows.rs`): Job Object containment of descendants of
  a suspended-start/assign/resume child, path checks (policy confinement plus
  held-handle identity and replacement detection), whole-job termination on
  cancellation with captured output preserved, and the merged stdout/stderr
  contract. Bounded termination while stdin is held open is the ignored Task 25
  skeleton (Windows has no SIGTERM).
- **TLS** (`tls_platform.rs`, every platform): enterprise CA as an added root,
  untrusted root rejected, root → intermediate → leaf accepted and the same
  chain without the intermediate rejected, hostname mismatch rejected, expired
  certificate rejected, and a root-trust case that differs per platform:
  Linux proves an **absent** root is rejected by the native bundle (Linux has
  no per-user distrust marker in this trust path), while macOS/Windows install
  the root in the platform store, prove trust, then prove explicit distrust.

## 2. Linux x86_64 evidence (actual)

Environment:

```console
$ uname -srm
Linux 7.0.0-43-generic x86_64
$ grep PRETTY_NAME /etc/os-release
PRETTY_NAME="Ubuntu 24.04.5 LTS"
$ rustc --version
rustc 1.94.1 (e408947bf 2026-03-25)
$ cargo --version
cargo 1.94.1 (29ea6fb6a 2026-03-24)
```

Command and result:

```console
$ cargo test --test platform_linux --test tls_platform -v
     Running unittests ... (platform_linux)
running 7 tests
test sigterm_with_stdin_open_exits_within_budget ... ignored, requires Task 25 serve lifecycle
test helper_bad_args_exit_126 ... ok
test openat2_bound_directory_denies_symlink_escape ... ok
test landlock_helper_restricts_writes ... ok
test wait_delay_nonzero_exit_precedence_when_descendant_holds_pipes ... ok
test wait_delay_zero_exit_when_descendant_holds_pipes ... ok
test process_group_cleanup_kills_backgrounded_descendant ... ok

test result: ok. 6 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out

     Running unittests ... (tls_platform)
running 8 tests
test backend_matches_the_platform ... ok
test absent_root_is_rejected_by_the_native_bundle ... ok
test expired_certificate_is_rejected ... ok
test enterprise_ca_extra_root_is_accepted ... ok
test untrusted_self_signed_leaf_is_rejected ... ok
test intermediate_chain_without_the_intermediate_is_rejected ... ok
test hostname_mismatch_is_rejected ... ok
test intermediate_chain_is_accepted ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

`SUBAGENT_MCP_REQUIRE_LANDLOCK=1` makes the Landlock case fail instead of skip;
the `platform-linux` CI job sets it. Without it (local runs) an unavailable
kernel prints a visible skip line.

Full workspace gates on the same host:

```console
$ cargo test --all-targets
# every target ok; platform_macos and platform_windows report 0 tests on Linux
$ cargo fmt --check
$ cargo clippy --all-targets -- -D warnings
# both clean
```

The Linux TLS root case is an **absent root** case, not an explicit distrust
marker: the test points `SSL_CERT_FILE` at a generated bundle that does **not**
contain the enterprise CA (rejected with `UnknownIssuer`) and then at a bundle
that does (accepted). That is the real system-root path for this platform, and
it does not modify the host's trust store. Linux has no per-user distrust store
in this trust path, so the explicit-distrust evidence is macOS `deny` /
Windows `Disallowed` only.

## 3. TLS trust matrix

`X` = covered and asserted by the test; `store` = covered by installing a
temporary root in the platform store (macOS/Windows only, see §4/§5).

| Case | Linux | macOS | Windows |
|---|---|---|---|
| Enterprise CA accepted | X (extra root + native bundle) | X (extra root) + store | X (extra root) + store |
| Root not trusted → rejected | X (absent from the native bundle) | store (`deny`) | store (`Disallowed`) |
| Intermediate chain accepted | X | X | X |
| Intermediate missing → rejected | X | X | X |
| Hostname mismatch rejected | X | X | X |
| Expired certificate rejected | X | X | X |

Certificates are generated in-process with `rcgen` for each run. Nothing is
committed and no real credential is used.

## 4. macOS evidence — PENDING NATIVE EVIDENCE

Not run. This environment is Linux-only. Run on a native macOS host (macOS 12+):

```console
$ rustup toolchain install 1.94.1 --profile minimal --component rustfmt --component clippy
$ cargo test --test platform_macos --test tls_platform -v
# 4 runtime tests pass, 1 ignored (SIGTERM skeleton); 7 TLS tests pass, 1 ignored
$ SUBAGENT_MCP_PLATFORM_TRUST_TESTS=1 \
    cargo test --test tls_platform -- --ignored -v
# platform_store_trust_and_distrust_apply passes (trust, then explicit distrust)
$ bash scripts/tls-test-fixtures.sh cleanup "subagent-mcp platform trust test CA"
```

The store case is `#[ignore]`d: it adds the generated root to the login keychain
(`security add-trusted-cert -r trustRoot`), proves `client_config()` accepts it,
re-adds it with `-r deny`, proves rejection, and removes it on drop. Running
`--ignored` without `SUBAGENT_MCP_PLATFORM_TRUST_TESTS=1` fails loudly instead
of touching the store. An ordinary run reports it as ignored, never as a pass.

Record in this section: `sw_vers`, `uname -m`, both commands, the full test
output, and the cleanup result.

## 5. Windows evidence — PENDING NATIVE EVIDENCE

Not run. Run on a native Windows host (Windows 10 / Server 2016+):

```powershell
rustup toolchain install 1.94.1 --profile minimal --component rustfmt --component clippy
cargo test --test platform_windows --test tls_platform -v
# 4 runtime tests pass, 1 ignored (Task 25 skeleton); 7 TLS tests pass, 1 ignored
$env:SUBAGENT_MCP_PLATFORM_TRUST_TESTS = "1"
cargo test --test tls_platform -- --ignored -v
certutil -delstore -user Root "subagent-mcp platform trust test CA" 2>$null
certutil -delstore -user Disallowed "subagent-mcp platform trust test CA" 2>$null
```

The store case installs the generated root in `CurrentUser\Root`, proves
`client_config()` accepts it, adds it to `CurrentUser\Disallowed`, proves
rejection, and removes it on drop. CurrentUser stores need no elevation.

Record in this section: `[System.Environment]::OSVersion.VersionString`,
`$env:PROCESSOR_ARCHITECTURE`, the command, and the full test output.

### Windows arm64

Use the GitHub-hosted `windows-11-arm` runner (the `platform-windows` matrix
entry). It runs the same `platform_windows` and `tls_platform` binaries natively;
a `cargo-xwin` cross-build is **not** acceptance and must not be recorded as a
pass.

If `windows-11-arm` is unavailable, run the same commands on a Windows arm64 VM
and record all of the following here:

1. the exact test command (`cargo test --test platform_windows --test tls_platform -v`),
2. the SHA-256 of the test binary that ran
   (`Get-FileHash target\debug\deps\platform_windows-*.exe -Algorithm SHA256`),
3. the Windows build number (`cmd /c ver` and `[System.Environment]::OSVersion.VersionString`),
4. the pass/fail result and full output.

## 6. Manual pre-release live smoke (not CI)

These are manual checks before a release, not CI jobs. Record the command,
provider/model, and outcome; never paste API keys, request bodies, or config
values.

### 6.1 Provider round trip

```console
# from a config with a real provider
$ SUBAGENT_MCP_CONFIG=/path/to/config.toml cargo run -- --check-config --live
```

Expected: the live round trip line reports success for each configured
provider, and no key value appears in stdout or stderr.

Status: **PENDING MANUAL SMOKE** (requires real provider credentials).

### 6.2 Codex app-server

```console
$ cargo test --test agent_session -- --nocapture   # fake/local app-server only
# manual: start the real Codex app-server and run one thread
```

Expected: thread start, one turn, and cleanup; the direct Codex child is killed
and reaped, and descendants may survive (documented baseline bound).

Status: **PENDING MANUAL SMOKE** (requires the Codex CLI).

### 6.3 Jev

```console
# manual: point the decision provider at the Jev endpoint and run one decision
```

Expected: the decision client builds with the platform TLS verifier and returns
a decision; no insecure fallback is used.

Status: **PENDING MANUAL SMOKE** (requires Jev access).

## 7. Fixtures and cleanup

- `scripts/tls-test-fixtures.sh generate <dir>` writes a one-day CA + leaf
  (`ca.pem`, `ca.key`, `leaf.pem`, `leaf.key`, `ca.cer`) for manual smoke runs.
- `install-ca` / `remove-ca` / `cleanup` mutate the platform trust store
  (macOS login keychain, Windows CurrentUser stores) and print the
  `SSL_CERT_FILE` export on Linux, where the native bundle is the store.
  `install-ca --deny` is unsupported on Linux and exits non-zero with an
  explanation (omit the CA from the bundle instead).
- The CI jobs run `cleanup` with `if: always()` so a failed test cannot leave a
  temporary root behind.
- The test root's common name is
  `subagent-mcp platform trust test CA`. It is not a real CA and is valid for
  one day.

## 8. Gate decision

- Linux x86_64: **verified** above (6 runtime tests + 8 TLS tests pass; the
  SIGTERM skeleton is ignored).
- Linux arm64, macOS x86_64/arm64, Windows x86_64, Windows arm64: **PENDING
  NATIVE EVIDENCE**. Fill the corresponding section with a native run before
  treating the platform gate as passed.
- Windows arm64 must be native (`windows-11-arm` or a Windows arm64 VM with the
  four recorded items in §5). A cross-build is not acceptance.
- The SIGTERM-with-stdin-open rows are **not** part of the gate until Task 25
  lands the serve lifecycle and removes `#[ignore]`. The OS trust-store row is
  gated by the dedicated `--ignored` CI step, not by the general matrix.
