# Tool reference

### `subagent`

Starts a new coding-agent thread.

| Parameter | Required | Default | Description |
|---|---:|---|---|
| `prompt` | Yes | — | String task prompt for the new thread. |
| `provider` | No (required when 2+ providers are configured and `model` is explicit) | The sole configured provider | Name of a provider defined under `[providers]`; the tool schema's `enum` lists them. Required at call time when the config defines more than one provider and `model` is not `auto`, otherwise optional. Fixed for the session's lifetime. |
| `model` | No | Selected provider's `default_model` | A model id from the selected provider's `models` list; the tool schema's `enum` is the union of every configured provider's models, described grouped by provider. When `[auto]` is configured, `auto` is also accepted: it lets the decision model choose the provider/model, so `provider` must be omitted. An unknown value is a tool error listing the selected provider's available ids. |
| `reasoning-effort` | No | `high` | One of the eight effort values; mapped through the provider's `effort_map` before it is sent. This argument wins over `config.model_reasoning_effort`, and an invalid value from either source is an error. |
| `cwd` | No | Server process working directory | Absolute path to an existing directory. |
| `sandbox` | No | `read-only` | `read-only`, `workspace-write`, or `danger-full-access`. |
| `approval-policy` | No | `on-request` | `untrusted`, `on-request`, `on-failure`, or `never`. |
| `base-instructions` | No | Built-in instructions | Complete replacement for the built-in base system instructions. An empty or omitted value uses the built-in default. |
| `developer-instructions` | No | None | Additional system instructions appended after the `AGENTS.md` blocks. An empty or omitted value appends nothing. |
| `config` | No | `{}` | Loose object. Recognized keys: `max_turns` (number from 1 to 100000, default 50, out-of-range or wrong-typed values are silently ignored), `model_reasoning_effort` (same values as `reasoning-effort`; the top-level argument wins), `max_nudges` (integer 0 to 10; overrides the provider's `max_nudges` for this session, other values are ignored), `writable_roots` (array of absolute paths to existing directories that the shell may also write under `workspace-write`), and the three budgets below. Invalid `model_reasoning_effort`, `writable_roots`, or budget values are errors; other unknown keys are silently ignored. |

The three budgets bound what one thread may spend. Each is checked before every model call, so a thread that has spent its budget is stopped rather than allowed one more turn:

| Key | Default | Meaning |
|---|---|---|
| `max_history_bytes` | `67108864` (64 MiB) | Ceiling on the history a thread replays. Every turn re-sends the whole history, so this bounds both resident memory and the prompt that grows with each turn. The default is far above any working thread — a 64k-token reply is roughly 256 KiB — and exists to stop runaway growth rather than to bound normal use. `0` disables it. |
| `max_total_tokens` | `0` (off) | Ceiling on the tokens the thread has reported across all its turns, taken from the providers' own usage counts. This is the cost ceiling: once the reported total reaches the budget, the next turn is refused. `0` disables it. Providers that run whole threads report usage through their streamed token events, so `codex-app-server` is covered too. |
| `run_deadline_seconds` | `0` (off) | Wall-clock ceiling on a single `subagent` or `subagent-reply` call. The one deadline covers every way a run blocks — a model request, an approval elicitation, a shell command — and the error names the deadline when it fires. `0` disables it. |

A run stopped by a budget returns an error naming the limit it reached and stays resumable: raise the limit on a new thread, or continue the existing one when only the deadline fired. The history and token ceilings are cumulative for the thread's life, so a thread that reaches one is stopped on every later reply as well.

When dispatched units run `go test` under `workspace-write`, include the absolute path of the Go build cache in `config.writable_roots` (for example `/home/<user>/.cache/go-build`).

The response includes `structuredContent.threadId`. Retain it to continue the session. Once a session is created, execution errors also return its `threadId`, so the session remains resumable. It also includes `provider` and `model`, the pair the session actually uses. When the model was selected by auto mode, `auto` carries `judge_model`, `confidence`, `probabilities` (keyed by provider/model, when available), `input_tokens`, and `output_tokens` for a decision model request. When only one candidate is available, `auto` instead carries `skipped: true` and omits the decision model fields. When providers are omitted, `auto` also carries their reasons in `omitted`. The `subagent-reply` output includes `threadId`, `content`, `provider`, and `model`, without `auto` selection details.

The system prompt is assembled as base instructions, then `AGENTS.md` blocks, then developer instructions. At session creation the server resolves `cwd` (after symlink evaluation), walks up to the nearest ancestor containing a `.git` entry, and collects `AGENTS.md` from that root down to `cwd`; without a repository root only `cwd` is read. Each file is wrapped in an `<agents_md path="...">` block, and the combined content is capped at 32 KiB with a truncation marker. A missing file is skipped, but any other read error fails the call: an `AGENTS.md` that is not a regular file, or a symlink that resolves outside the repository root, fails the `start` call instead of being read, so a repository cannot point it at a private key.

Across turns the server replays each provider's reasoning state exactly as that API requires (DeepSeek `reasoning_content`, OpenAI encrypted reasoning items, Anthropic signed thinking blocks); rollout files never record those payloads.

### `subagent-reply`

Continues an existing coding-agent thread. Session settings cannot be changed on a reply.

| Parameter | Required | Description |
|---|---:|---|
| `threadId` | Yes | String thread ID returned by an earlier `subagent` or `subagent-reply` call. |
| `prompt` | Yes | String follow-up prompt for the existing thread. |

Only one call can process a thread at a time. A concurrent reply returns a `busy` error with the same `threadId`. An unknown ID returns `unknown threadId`; check the ID or create a new session if the server has restarted.

### Agent tools

The agent runs inside the thread with four built-in tools:

Their JSON parameter schemas are sent to the model as written: every keyword is forwarded, so a schema with `additionalProperties`, `$defs`, `oneOf` or `title` reaches the provider intact. A schema that is not a JSON object fails the turn naming the tool, rather than the model being handed a tool with no parameters.

| Tool | Arguments | Description |
|---|---|---|
| `shell` | `command`, optional `timeout_seconds`, optional `justification` | Run a bash command in `cwd`. The default timeout is 60 seconds and requests are clamped to 600 seconds. Combined stdout and stderr is capped at 16 KiB: the start is kept, the end is dropped, and `[output truncated: N bytes total]` is appended. When the call returns, its process group is killed, so work the command backgrounded does not outlive the call: a `&` job is for parallelism inside one command, not for starting something that keeps running. |
| `read_file` | `path`, optional `offset`, optional `limit`, optional `justification` | Read a file; relative paths resolve against `cwd`. `offset` is a 1-based start line and `limit` caps the number of lines. Output is capped at 16 KiB of whole lines; truncation appends `[content truncated: N bytes total; continue with offset=K]`, a line limit appends `[more lines follow; continue with offset=K]`, and an offset past the end returns `[offset K is past the end of the file (N lines)]`. A path that is not a regular file — a FIFO, device, socket, or directory — is refused instead of read. |
| `write_file` | `path`, `content`, optional `justification` | Create or overwrite a whole file, creating parent directories. An existing target that is not a regular file (a FIFO, device, socket, or directory) is refused. |
| `apply_patch` | `patch`, optional `justification` | Edit files with a Codex-format patch. |

`apply_patch` accepts a patch from `*** Begin Patch` to `*** End Patch` containing one or more `*** Add File: <path>` (with `+` content lines), `*** Delete File: <path>`, or `*** Update File: <path>` sections, an optional `*** Move to: <path>` after an update header, `@@` chunk headers, and `' '` context, `-` removed, and `+` added lines. `*** End of File` anchors a chunk at the end of the file. Leading and trailing whitespace around the whole patch, a `<<'EOF'` heredoc wrapper, and CRLF line endings are tolerated. The whole patch is parsed and matched in memory first: if any chunk fails to match, or an Add targets an existing file, nothing is written. An existing file the patch reads or overwrites must be a regular file no larger than 64 MiB: a device, FIFO, socket, directory, or a bigger file is refused before anything is written. Sections for the same file apply in order, each seeing the edits of the earlier ones (an impossible combination, such as an update after a delete, is rejected up front), and a failure while writing restores every file already touched. Context matching tries exact lines, then ignores trailing whitespace, then ignores surrounding whitespace. The tool result starts with `Success. Updated the following files:` followed by `A`, `M`, or `D` plus the path of each change.

### Codex-compatible tool names

Claude Code names MCP tools `mcp__<server name>__<tool name>`. Register the server under the name `codex` and set `SUBAGENT_MCP_TOOL_NAME=codex`, and instructions written for Codex keep working: the tools are exposed as `mcp__codex__codex` and `mcp__codex__codex-reply`.

```json
{
  "mcpServers": {
    "codex": {
      "type": "stdio",
      "command": "subagent-mcp",
      "env": {"SUBAGENT_MCP_TOOL_NAME": "codex", "DEEPSEEK_API_KEY": "..."}
    }
  }
}
```

With the default `SUBAGENT_MCP_TOOL_NAME=subagent`, the tools are `subagent` and `subagent-reply` (as `mcp__subagent__subagent` and `mcp__subagent__subagent-reply`).

[Back to README](../../README.md)
