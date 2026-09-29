---
description: Verify the subagent-mcp installation (binary on PATH, config file loads, each provider's key and reachability)
allowed-tools: Bash
---

Verify the local subagent-mcp installation and print a compact status report.

`subagent-mcp --check-config` validates the config file, reports each provider's
`env_key` name and whether it is set, and asks each configured provider for its
model list so a bad key or base URL fails loudly. It never prints key values.

Run these checks with Bash:

1. Locate and identify the binary:

   ```bash
   command -v subagent-mcp && subagent-mcp --version
   ```

   If the binary is missing, tell the user to run `go install ./cmd/subagent-mcp`
   from the subagent-mcp repository root and make sure `$(go env GOPATH)/bin` is
   on PATH.

2. Validate the config file and each provider:

   ```bash
   config="${SUBAGENT_MCP_CONFIG:-$HOME/.config/subagent-mcp/config.toml}"
   echo "config file: $config"
   if subagent-mcp --check-config "$config"; then
     echo "config status: PASS"
   else
     echo "config status: FAILED (exit $?)"
   fi
   ```

   Exit code 0 means at least one configured provider has its key set and every
   check that ran passed; exit code 1 means either no configured provider has
   its key set, or a provider's API call or model check actually failed. A
   missing key is always reported as `not set, skipped` and does not by itself
   fail the run. A provider whose API call fails is reported as `FAIL` with the
   error. Warnings that a configured model id is not in the provider's current
   model list do not affect the exit code.

   The output also has a `sandbox  Landlock` line. `OK` means auto-allowed shell
   calls run under the kernel sandbox; `WARN` (macOS or Linux below 5.13) means
   the native file tools work but every auto-allowed `shell` call exits with
   code 126. It does not affect the exit code, but report it.

   Do not add `--live` here: it makes real, billed API calls. Mention it only if
   the user asks how to verify a provider end to end.

3. If the config file is missing, tell the user to copy `config.example.toml`
   from the subagent-mcp repository to the path from step 2, set at least one
   provider's `env_key`, and reconnect the MCP server.

Then report:

- subagent-mcp binary: <absolute path and version, or MISSING with the install hint>
- config file: <path, and OK, MISSING with the copy hint, or FAILED with the failing check-config line>
- sandbox: <the `sandbox  Landlock` line: OK, or the WARN with its consequence>
- providers: <for each provider in the check-config output: name, key name set
  or skipped, api OK or FAIL; never key values>
- ready: <when the binary, the config, and at least one provider's key all
  pass: the MCP tools are available once the server is connected — `subagent`
  and `subagent-reply` by default, or the names set by SUBAGENT_MCP_TOOL_NAME.
  With more than one provider configured, note that each `subagent` call must
  pass `provider` to choose which one, unless only one is configured;
  otherwise report the exact step to fix>
