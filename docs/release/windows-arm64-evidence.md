# Windows arm64 release evidence (Task 29A)

Status: **PENDING NATIVE EVIDENCE. No Windows arm64 archive has been accepted
yet.**

This file records how the Windows arm64 release artifact is produced and what
must be recorded before it can be treated as a release asset. The acceptance
path is a native build and native run on Windows arm64. A `cargo-xwin`
cross-build is a build-path fallback only; it is **not** runtime evidence.

- Workflow: `.github/workflows/rust-release-dry-run.yml`
- Packaging script: `scripts/package-rust-release.sh`
- Platform runtime gate (separate, Task 28):
  `docs/release/live-smoke-evidence.md`
- Design reference: `docs/superpowers/specs/2026-11-09-rust-rewrite-design.md`
  §8.5 (release matrix)

The dry-run workflow never tags or publishes and never updates
`scripts/release.lock` or the plugin pin.

## 1. Six-target matrix

All six archives are produced by one build each, then `checksums.txt` is
generated from those exact archives in a separate job that only downloads them.

| Target | Runner | Build | Archive |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | `ubuntu-24.04` | native, musl | `subagent-mcp_<version>_linux_amd64.tar.gz` |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | native, musl | `subagent-mcp_<version>_linux_arm64.tar.gz` |
| `x86_64-apple-darwin` | `macos-15-intel` | native, `MACOSX_DEPLOYMENT_TARGET=12.0` | `subagent-mcp_<version>_darwin_amd64.tar.gz` |
| `aarch64-apple-darwin` | `macos-15` | native, `MACOSX_DEPLOYMENT_TARGET=12.0` | `subagent-mcp_<version>_darwin_arm64.tar.gz` |
| `x86_64-pc-windows-msvc` | `windows-2022` | native, `+crt-static` | `subagent-mcp_<version>_windows_amd64.zip` |
| `aarch64-pc-windows-msvc` | `windows-11-arm` | native, `+crt-static` | `subagent-mcp_<version>_windows_arm64.zip` |

Windows archives contain `subagent-mcp.exe`; Unix archives contain
`subagent-mcp`. Every archive also contains `README.md` and
`config.example.toml`, and the packaging script refuses to finish if the entry
set or a non-empty size does not match.

The Windows arm64 artifact additionally carries a `WINDOWS_ARM64_BUILD_PATH`
marker (`native` or `cargo-xwin`) written by the job that built it. The
`checksums` job copies that marker into the dry-run artifact, writes
`arm64-build-status.json` (`target`, `event`, `release_gate`, `build_path`,
`archive_present`, `acceptance`), and fails the run when the marker says
`cargo-xwin`. A six-archive set is required on `workflow_dispatch`; on
`pull_request` a missing arm64 archive is a warning so the preview
`windows-11-arm` label cannot block packaging PRs.

## 2. Native Windows arm64 path (acceptance)

Job: `windows_arm64_native` on `windows-11-arm`, running the same packaging
script as the other five targets. On `workflow_dispatch` it is a required gate;
on `pull_request` it is `continue-on-error` and a missing archive is recorded as
a warning, because the runner label is a preview.

Evidence to record after the first successful run:

| Field | Value |
|---|---|
| Date | |
| Workflow run URL | |
| Archive name | |
| Archive SHA-256 (`Get-FileHash <zip> -Algorithm SHA256`) | |
| `subagent-mcp.exe` SHA-256 after extraction | |
| `$env:PROCESSOR_ARCHITECTURE` (must be `ARM64`) | |
| Windows build (`[System.Environment]::OSVersion.Version`) | |
| `subagent-mcp.exe --version` output | |
| `subagent-mcp.exe --version` exit code (must be 0) | |
| Result (pass/fail) | |

Commands on the native runner or VM:

```powershell
$zip = 'subagent-mcp_<version>_windows_arm64.zip'
Get-FileHash $zip -Algorithm SHA256
Expand-Archive $zip -DestinationPath pkg -Force
Get-FileHash pkg\subagent-mcp.exe -Algorithm SHA256
$env:PROCESSOR_ARCHITECTURE
[System.Environment]::OSVersion.Version
.\pkg\subagent-mcp.exe --version
$LASTEXITCODE
```

Only `--version` exists in the current CLI. The MCP stdio handshake and the
server lifecycle belong to Task 25 (`cli::serve`); add handshake evidence here
before any Windows arm64 archive is published. Until then, a passing
`--version` on ARM64 proves the binary is the right architecture and starts
under Windows arm64 — nothing more.

## 3. `cargo-xwin` fallback path

Use only if the `windows-11-arm` runner label is unavailable. Re-run the
workflow manually with `windows_arm64_fallback=true`; job `windows_arm64_xwin`
then cross-builds on `ubuntu-24.04` with `cargo-xwin` and
`CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_RUSTFLAGS=-C target-feature=+crt-static`.

A cross-built archive is **not acceptance**. The fallback job writes
`dist/WINDOWS_ARM64_BUILD_PATH=cargo-xwin`; the `checksums` job copies that
marker into the artifact, sets `"acceptance": "requires-vm-evidence"` in
`arm64-build-status.json`, and fails the run. The runtime checklist below is
mandatory and must be filled in before the artifact can be treated as a release
candidate.

| Field | Value |
|---|---|
| Date | |
| Workflow run URL (fallback job) | |
| Archive name | |
| Archive SHA-256 | |
| Extracted `subagent-mcp.exe` SHA-256 | |
| VM / host provider and image | |
| `$env:PROCESSOR_ARCHITECTURE` (must be `ARM64`) | |
| Windows build number | |
| `subagent-mcp.exe --version` output | |
| `subagent-mcp.exe --version` exit code (must be 0) | |
| Binary architecture confirmed (`dumpbin /headers` or `Get-Item` on ARM64) | |
| Result (pass/fail) | |

Minimum acceptable evidence is a real Windows arm64 machine (VM is fine) where
the extracted cross-built `subagent-mcp.exe` prints the expected version and
exits 0, with the machine architecture recorded as ARM64. A cross-build that
was only inspected on the Linux builder does not satisfy this gate.

## 4. Out of scope for Task 29A

`scripts/perf-smoke.sh` and `docs/release/perf-results.md` are **Task 29B** and
depend on the Task 25 server. This task does not create, run, or record any
performance results, and this workflow contains no performance gate.
