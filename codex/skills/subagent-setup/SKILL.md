---
name: subagent-setup
description: Verify the subagent-mcp Codex plugin installation (binary, config file, each provider's key and reachability, and whether Codex forwards each key to the MCP server). Use when the user asks to check, verify, or troubleshoot the subagent-mcp install, or when the subagent MCP tools are missing.
---

# subagent-mcp setup check (Codex)

The plugin root is the directory three levels above this file. Read `commands/setup.md` in the plugin root and follow its checks and report format, with the Codex differences below. Never print key values.

## Binary

The Codex plugin does not put `subagent-mcp` on PATH. Instead of `command -v subagent-mcp`, use the first of:

1. `$SUBAGENT_MCP_BIN`, when set.
2. `${XDG_CACHE_HOME:-$HOME/.cache}/subagent-mcp/bin/<version>/subagent-mcp`, where `<version>` comes from the first line (`version <x.y.z>`) of `scripts/release.lock` in the plugin root.

If neither exists, run `sh <plugin root>/scripts/launch.sh --version`. It downloads the pinned release, verifies its checksum, installs it into that cache path, and prints the version or the launcher error. It needs network access and write access to the cache directory; request escalation if the sandbox blocks it. Use the resulting path as the binary for the remaining checks.

`subagent-mcp --check-config` calls each provider's API, so it also needs network access.

## Environment forwarded by Codex

The shell you run in sees the user's environment, but Codex starts the MCP server with a minimal environment plus only the names in the `env_vars` list of `.codex-mcp.json` in the plugin root. Read that list. For each provider in the check-config output whose key source is an `env_key`, report whether that name is in the list. A key that is set in the shell but not listed is not seen by the server: tell the user to rename the provider's `env_key` to a listed name or use `key` in the config file. `SUBAGENT_MCP_CONFIG` is forwarded, so a custom config path set before starting Codex applies to the server too.

## MCP server

Run `codex mcp list` and report the `subagent` row (status `enabled` expected). If the binary and config pass but the tools are still missing, tell the user to start a new Codex session after exporting the keys, since the server reads its environment and config only at startup.

## Report

Use the report from `commands/setup.md`, add `forwarded: yes|no` to each provider line and a `codex mcp:` line with the `subagent` status. In Codex the tools appear as `mcp__subagent__subagent` and `mcp__subagent__subagent_reply` (or the names set by `SUBAGENT_MCP_TOOL_NAME`).
