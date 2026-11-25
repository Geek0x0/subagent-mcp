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

The differential tests need the pinned reference binary; see [the reusable CI gates](../.github/workflows/rust-gates.yml) for its setup and platform checks. Rust does not use the former Go `-race` test command.

The release archives are built with `scripts/package-rust-release.sh` and contain the binary, `README.md`, and `config.example.toml`. The README links to online reference documents so those links remain usable outside a repository checkout.

After publishing a release, the `sync-plugin-versions` job in `release.yml` pins `scripts/release.lock` and both plugin `version` fields to the tag and pushes them to `main`. This is the only writer of the plugin manifests: when bumping the crate version update `Cargo.toml`, `Cargo.lock`, `src/lib.rs` (`VERSION`) and the CLI test expectations, but leave `.claude-plugin/plugin.json` and `.codex-plugin/plugin.json` alone. The `plugin manifests are CI-owned` gate rejects any push or PR that changes them unless the pusher is the Actions bot, and the `plugin manifest versions` gate keeps the two files agreeing with each other. If the sync job fails, re-run it from Actions instead of editing the files by hand. Use `scripts/update-release-lock.sh <version>` by hand only to re-pin an existing release whose checksums changed or whose lock was missed.

The plugin ships two manifests: `.claude-plugin/plugin.json` for Claude Code and `.codex-plugin/plugin.json` for Codex. Codex does not expand `${CLAUDE_PLUGIN_ROOT}`, so it reads its MCP server from `.codex-mcp.json` (relative command, `cwd` set to the plugin root) instead of `.mcp.json`, and only forwards environment variables listed in that file's `env_vars`. Codex does not load `commands/`, so `codex/skills/subagent-setup` wraps `commands/setup.md` with the Codex differences; it lives outside `skills/` so Claude Code does not load it alongside `/subagent:setup`.

[Back to README](../README.md)
