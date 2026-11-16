# Development and releases

Build from source with Rust 1.94.1:

```bash
cargo build --release --locked
```

The binary is `target/release/subagent-mcp`. `go install` is no longer supported after 0.14.0.

Run the Rust tests that do not require the pinned Go reference binary:

```bash
cargo test --all-targets -- --skip differential_
```

The differential tests need the pinned reference binary; see [the CI workflow](../.github/workflows/rust-ci.yml) for its setup and platform checks. Rust does not use the former Go `-race` test command.

The release archives are built with `scripts/package-rust-release.sh` and contain the binary, `README.md`, and `config.example.toml`. The README links to online reference documents so those links remain usable outside a repository checkout.

After publishing a release, run `scripts/update-release-lock.sh <version>` and commit `scripts/release.lock` so the plugin launcher picks up the new binary.

[Back to README](../README.md)
