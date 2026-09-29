# subagent-mcp

subagent-mcp is one MCP server binary that exposes a coding agent — real shell and file-tool execution, a Landlock write sandbox, approvals, rollout files, cancellation, and progress notifications — driven by any of three model API families, or delegates whole sessions to Codex through its app-server. The MCP caller selects which configured provider a session uses via the `provider` argument.

| `api` value | Wire protocol | Example providers |
|---|---|---|
| `chat-completions` | OpenAI Chat Completions, streaming SSE | DeepSeek, any OpenAI-compatible endpoint |
| `responses` | OpenAI Responses API, streaming SSE | OpenAI |
| `messages` | Anthropic Messages API, streaming SSE | Anthropic |
| `codex-app-server` | Codex app-server, JSON-RPC over stdio | Local Codex CLI (experimental) |

## Install

1. Install the binary:

   ```bash
   go install github.com/Geek0x0/subagent-mcp/cmd/subagent-mcp@latest
   ```

   Alternatively, from a local clone of the repository, run:

   ```bash
   go install ./cmd/subagent-mcp
   ```

   Make sure `$(go env GOPATH)/bin` is on `PATH`.

   Linux is the supported platform. The kernel sandbox needs Landlock (Linux 5.13 or newer); on macOS or an older kernel the server still starts and the native `read_file`, `write_file`, and `apply_patch` tools work, but every auto-allowed `shell` call exits with code 126 rather than run unsandboxed (see [Kernel sandbox for shell calls](#kernel-sandbox-for-shell-calls)). `--check-config` prints a `sandbox  Landlock` line that says which case applies.

2. Add this repository as a Claude Code plugin marketplace:

   ```bash
   claude plugin marketplace add Geek0x0/subagent-mcp
   ```

   Alternatively, if you already have the repository checked out locally, run:

   ```bash
   claude plugin marketplace add /path/to/subagent-mcp
   ```

3. Install the plugin:

   ```bash
   claude plugin install subagent@subagent-mcp
   ```

4. Create the config file and provide each provider's key you plan to use in the environment that launches the MCP server (for Codex, use `codex login` instead):

   ```bash
   mkdir -p ~/.config/subagent-mcp
   cp config.example.toml ~/.config/subagent-mcp/config.toml
   export DEEPSEEK_API_KEY=...   # the variable named by that provider's env_key
   ```

   Restart or reconnect the MCP server after changing the config, then run `/subagent:setup` to verify the binary, the config file, and the key.

## Configuration

The TOML file is loaded once at startup from `$SUBAGENT_MCP_CONFIG`, or from `~/.config/subagent-mcp/config.toml` when that variable is unset. A missing file fails startup with a message naming the path and pointing at `config.example.toml`; the repository's `config.example.toml` has a ready-to-edit deepseek/openai/anthropic setup. Decoding is strict, so a typo is an error rather than a silently ignored key, and every validation error names the offending field path (for example `providers.openai.default_model`).

```toml
[providers.deepseek]
api = "chat-completions"
base_url = "https://api.deepseek.com"
env_key = "DEEPSEEK_API_KEY"
default_model = "deepseek-flash"
models = [
  { id = "deepseek-flash",  description = "fast, cheap" },
  { id = "deepseek-v4-pro", description = "strongest" },
]
effort_map = { medium = "high", xhigh = "max" }

[providers.openai]
api = "responses"
env_key = "OPENAI_API_KEY"
default_model = "gpt-5.5"
models = [{ id = "gpt-5.5" }]

[providers.anthropic]
api = "messages"
env_key = "ANTHROPIC_API_KEY"
default_model = "claude-sonnet-5"
max_output_tokens = 64000
models = [{ id = "claude-sonnet-5" }, { id = "claude-opus-5" }]
```

### Fields

| Field | Level | Required | Rules |
|---|---|---|---|
| `api` | provider | yes | One of `chat-completions`, `responses`, `messages`, `codex-app-server`. |
| `base_url` | provider | no | Defaults per api: `https://api.openai.com/v1` for `chat-completions` and `responses`, `https://api.anthropic.com` for `messages`. Rejected when non-empty for `codex-app-server`, which has no URL default. |
| `env_key` | provider | except Codex | Name of the environment variable holding the key. Its value is read when a session is created for that provider (via the `provider` argument, or the sole configured provider); a missing value fails that call naming the variable and `providers.<name>.env_key`. Rejected when non-empty for `codex-app-server`: authentication goes through `codex login`. |
| `command` | provider | no | Executable name or path for `codex-app-server`; defaults to `codex` on `PATH`. The server adds the `app-server` argument; this is not a shell command or argument list. Rejected when non-empty for every other api. |
| `default_model` | provider | yes | Must be one of `models[].id`. Used when a caller omits `model`. |
| `models` | provider | yes | Non-empty list of `{ id, description? }` tables; ids must be unique and are advertised in config order. |
| `effort_map` | provider | no | Maps a caller effort value to the value sent to the API: `effort_map[value]` if present, else the caller's value. Keys and values must be in the effort set below. |
| `max_output_tokens` | provider | no | Positive integer; defaults to `64000` for every api but is only sent by `messages` as `max_tokens`. |

### Reasoning effort

Accepted caller values are `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`; the default is `high`. The value sent to the provider is `effort_map[value]` when the selected provider defines that mapping, otherwise the caller's value. The server validates against the seven values above and does not otherwise check provider-specific support, so an unsupported value surfaces as the provider's API error. `config.model_reasoning_effort` in the `config` object works like `reasoning-effort`, and the top-level argument wins.

### API keys

Keys are never stored in the config file. For the three model API families, when a session is created, the server reads the selected provider's `env_key` variable from its own environment; every provider's `env_key` variable is also removed from shell commands run by the agent (see [Environment](#environment)). Keep the variable in the environment that launches the MCP server, for example in an MCP client's `env` block. Do not rely on a shell profile to provide the key to the agent: login profiles are loaded for shell commands, but configured keys are removed again before the command runs. Codex uses its own login instead.
Requests to a provider's `base_url` carry only that provider's configured key and the SDK's standard protocol headers: the SDKs' own credential environments (`ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_CUSTOM_HEADERS`, `OPENAI_ORG_ID`, `OPENAI_PROJECT_ID`, `OPENAI_CUSTOM_HEADERS`, and the `*_BASE_URL`/`*_API_KEY` variables) are ignored, so gateway tokens and headers from the server's environment are never forwarded to a configured endpoint.

### Selecting a provider

Pass `provider` when starting a session to choose which configured provider it uses; the tool schema's `enum` lists every name defined under `[providers]`. Omitting `provider` works only when the config defines exactly one provider — with two or more, omitting it is a tool error listing the available names. The chosen provider is fixed for the session's lifetime: `subagent-reply` always continues with the same one, and each call's `model` must come from that provider's list.

### Codex app-server provider

`codex-app-server` is **experimental**, tested with **codex-cli 0.156.1**. Install Codex, run `codex login` as the user launching the MCP server, and enable the commented `[providers.codex]` block in `config.example.toml`. Set `command` only if the executable is not `codex` on `PATH`; omit `env_key` and `base_url`.

The session inherits the user's whole Codex environment — auth, config, plugins, hooks, skills, MCP servers, and AGENTS.md — and uses **Codex's own tools and sandbox**, not subagent-mcp's native four-tool runner or Landlock policy. The app-server child still receives an environment scrubbed of configured `env_key` variables and `SUBAGENT_MCP_*` variables. The native tool, prompt-loading, sandbox, and rollout details elsewhere in this README apply to the three model API families; Codex differs as follows:

- `config.max_turns` is ignored; Codex manages the agent loop.
- AGENTS.md is loaded by Codex, not assembled into the prompt by subagent-mcp.
- `base-instructions` replaces Codex's base prompt only when given a non-empty value; omitting it leaves Codex's own base prompt intact. `developer-instructions` is forwarded separately.
- Codex owns the non-ephemeral thread and its rollout under `~/.codex/sessions` (or the user's configured Codex home). It appears in Codex history; subagent-mcp does not write a second rollout, and `SUBAGENT_MCP_ROLLOUT=off` does not disable Codex's rollout.
- `sandbox` is forwarded to Codex; `approval-policy` is forwarded with `on-failure` mapped to `on-request`. Command and file-change approvals are relayed to the MCP client. `config.writable_roots` is forwarded only for `workspace-write`.
- The relayed approval's target is the shell command, the touched paths for a file change, or — for a network approval — the requested host. Every other request Codex sends to the server (an MCP server's elicitation, a tool user-input request, a permission request, a dynamic tool call, or anything else) is answered automatically with the error `-32601 not supported by subagent-mcp`, so Codex's MCP servers and tools cannot prompt through subagent-mcp; those requests fail inside Codex instead of reaching your MCP client.
- A pending approval wait ends when the call is cancelled or the five-minute approval timeout expires (denial), and also when the Codex child process dies or the thread closes, so a pending approval never outlives either.
- Codex sends `item/started` before it asks for approval, so a declined file change still emits `exec_command_begin` and `exec_command_end`; see [Events](#events).

## Validating the config

`--check-config` loads and validates the config file, reports whether the Landlock kernel sandbox is usable on this machine (a `WARN` there does not change the exit code), reports each model API provider's `env_key` name and whether it is set (or Codex's `auth` status), and asks each reachable provider for its model list so authentication and reachability errors fail loudly and unlisted model ids are warned about:

```bash
subagent-mcp --check-config [path]
subagent-mcp --check-config --live [path]
```

The optional `path` defaults to `$SUBAGENT_MCP_CONFIG` or `~/.config/subagent-mcp/config.toml`. Flags must come before the path. The process exits 0 unless a provider check actually failed or no provider has a key set or passes Codex auth. A missing config, unknown TOML key, or invalid field is reported naming the path or field. A missing API key is skipped, but a Codex auth failure counts as failed. Output looks like:

```
config   /home/you/.config/subagent-mcp/config.toml   OK
provider anthropic (messages)
  key    ANTHROPIC_API_KEY   not set, skipped
provider deepseek (chat-completions)
  key    DEEPSEEK_API_KEY   set
  api    https://api.deepseek.com   OK (2 models listed)
  model  deepseek-flash     OK
  model  deepseek-v4-pro     WARN: not in the provider's current model list
result   PASS (1 checked, 1 skipped, 0 failed)
```

If no configured provider has a key set or passes Codex auth, and none failed, the result line instead reads `result   FAIL (0 checked, N skipped, 0 failed) — no configured provider has its key set`.

For a Codex-only config, an illustrative `--check-config` result is:

```text
config   /home/you/.config/subagent-mcp/config.toml   OK
provider codex (codex-app-server)
  auth   chatgpt (plus)   OK
  api    codex-cli 0.156.1   OK (7 models listed)
  model  gpt-6-astra     OK
  model  gpt-6-sol     OK
  model  gpt-5.5     OK
result   PASS (1 checked, 0 skipped, 0 failed)
```

The `auth` line replaces `key`: the server starts Codex app-server and calls `account/read`, then `model/list` for the `api` and `model` lines. Account type, plan, version, and model count depend on the installation. A missing login prints `  auth   FAIL: not logged in; run codex login`; a process-start error also prints `  auth   FAIL: <error>`. A model-list failure prints `  api    FAIL: <error>`. Passing auth counts as checked; an auth or API failure counts as failed.

`model ... WARN` means a configured model id is not in the provider's current list; it does not affect the exit code. `--live` is opt-in and makes one real, billed tool-call round trip per reachable model API provider using `default_model` and effort `low`. For Codex it instead runs one ephemeral, read-only thread with approval policy `never`, asking for the sum of two numbers — the total never appears in the prompt — and checking that the final answer contains that total, so a reply that merely quotes the prompt fails. Its line is `  live   <default_model>   OK (turn succeeded, <duration>)` or `  live   <default_model>   FAIL: <error>`. Both paths print the billed-call warning first; use `--live` deliberately, not in CI. Neither mode ever prints a key value.

## Environment

| Variable | Description |
|---|---|
| `SUBAGENT_MCP_CONFIG` | Config file path; defaults to `~/.config/subagent-mcp/config.toml`. |
| `SUBAGENT_MCP_TOOL_NAME` | Base name for the two MCP tools; defaults to `subagent`, exposing `subagent` and `subagent-reply`. `codex` registers `codex` and `codex-reply`. The value must match `^[A-Za-z0-9_-]{1,64}$` and must not end with `-reply`; an invalid value fails startup naming `SUBAGENT_MCP_TOOL_NAME`. |
| `SUBAGENT_MCP_ROLLOUT` | Set to `off` to disable rollout files. |

Before running any shell command, the server removes from the command environment every variable named by any provider's `env_key` and every variable whose name starts with `SUBAGENT_MCP_`, including values a login profile tries to re-export. The command still runs through `bash -lc`, so profile-provided settings such as `PATH` work; cleanup happens after the profile is loaded. On Linux, the server also starts non-dumpable, preventing a same-user shell child from reading the server's environment through `/proc`. `read_file` additionally refuses to read the loaded config file, resolved through symlinks, to keep provider topology out of model context. Reads are otherwise unrestricted: a command can still read a key stored in another file, so prefer passing keys through the MCP server environment rather than keeping them on disk.

## Tools

### `subagent`

Starts a new coding-agent thread.

| Parameter | Required | Default | Description |
|---|---:|---|---|
| `prompt` | Yes | — | String task prompt for the new thread. |
| `provider` | No (required when 2+ providers are configured) | The sole configured provider | Name of a provider defined under `[providers]`; the tool schema's `enum` lists them. Required when the config defines more than one, otherwise optional. Fixed for the session's lifetime. |
| `model` | No | Selected provider's `default_model` | A model id from the selected provider's `models` list; the tool schema's `enum` is the union of every configured provider's models, described grouped by provider. An unknown value is a tool error listing the selected provider's available ids. |
| `reasoning-effort` | No | `high` | One of the seven effort values; mapped through the provider's `effort_map` before it is sent. This argument wins over `config.model_reasoning_effort`, and an invalid value from either source is an error. |
| `cwd` | No | Server process working directory | Absolute path to an existing directory. |
| `sandbox` | No | `read-only` | `read-only`, `workspace-write`, or `danger-full-access`. |
| `approval-policy` | No | `on-request` | `untrusted`, `on-request`, `on-failure`, or `never`. |
| `base-instructions` | No | Built-in instructions | Complete replacement for the built-in base system instructions. An empty or omitted value uses the built-in default. |
| `developer-instructions` | No | None | Additional system instructions appended after the `AGENTS.md` blocks. An empty or omitted value appends nothing. |
| `config` | No | `{}` | Loose object. Recognized keys: `max_turns` (number from 1 to 100000, default 50, out-of-range or wrong-typed values are silently ignored), `model_reasoning_effort` (same values as `reasoning-effort`; the top-level argument wins), and `writable_roots` (array of absolute paths to existing directories that the shell may also write under `workspace-write`). Invalid `model_reasoning_effort` or `writable_roots` values are errors; other unknown keys are silently ignored. |

When dispatched units run `go test` under `workspace-write`, include the absolute path of the Go build cache in `config.writable_roots` (for example `/home/<user>/.cache/go-build`).

The response includes `structuredContent.threadId`. Retain it to continue the session. Once a session is created, execution errors also return its `threadId`, so the session remains resumable.

The system prompt is assembled as base instructions, then `AGENTS.md` blocks, then developer instructions. At session creation the server resolves `cwd` (after symlink evaluation), walks up to the nearest ancestor containing a `.git` entry, and collects `AGENTS.md` from that root down to `cwd`; without a repository root only `cwd` is read. Each file is wrapped in an `<agents_md path="...">` block, and the combined content is capped at 32 KiB with a truncation marker. A missing file is skipped, but any other read error fails the call.

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

| Tool | Arguments | Description |
|---|---|---|
| `shell` | `command`, optional `timeout_seconds`, optional `justification` | Run a bash command in `cwd`. The default timeout is 60 seconds and requests are clamped to 600 seconds. Combined stdout and stderr is capped at 16 KiB: the start is kept, the end is dropped, and `[output truncated: N bytes total]` is appended. |
| `read_file` | `path`, optional `offset`, optional `limit`, optional `justification` | Read a file; relative paths resolve against `cwd`. `offset` is a 1-based start line and `limit` caps the number of lines. Output is capped at 16 KiB of whole lines; truncation appends `[content truncated: N bytes total; continue with offset=K]`, a line limit appends `[more lines follow; continue with offset=K]`, and an offset past the end returns `[offset K is past the end of the file (N lines)]`. |
| `write_file` | `path`, `content`, optional `justification` | Create or overwrite a whole file, creating parent directories. |
| `apply_patch` | `patch`, optional `justification` | Edit files with a Codex-format patch. |

`apply_patch` accepts a patch from `*** Begin Patch` to `*** End Patch` containing one or more `*** Add File: <path>` (with `+` content lines), `*** Delete File: <path>`, or `*** Update File: <path>` sections, an optional `*** Move to: <path>` after an update header, `@@` chunk headers, and `' '` context, `-` removed, and `+` added lines. `*** End of File` anchors a chunk at the end of the file. Leading and trailing whitespace around the whole patch, a `<<'EOF'` heredoc wrapper, and CRLF line endings are tolerated. The whole patch is parsed and matched in memory first: if any chunk fails to match, or an Add targets an existing file, nothing is written. Sections for the same file apply in order, each seeing the edits of the earlier ones (an impossible combination, such as an update after a delete, is rejected up front), and a failure while writing restores every file already touched. Context matching tries exact lines, then ignores trailing whitespace, then ignores surrounding whitespace. The tool result starts with `Success. Updated the following files:` followed by `A`, `M`, or `D` plus the path of each change.

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

## Provider notes

### `chat-completions`

Streams OpenAI Chat Completions with `include_usage` and reassembles tool-call fragments into the calls the server sent, keyed by index when the server provides one and by id when it omits indices; a call that ends the stream without an id or name fails the turn instead of being sent onward with an empty `tool_call_id`. A `finish_reason` of `length` or `content_filter`, and a stream that ends without any finish reason fail the turn; other finish reasons (including compatible servers' `eos` and unrecognised ones, which are logged) count as finished rather than return truncated or filtered content. Only stream setup is retried; a mid-stream failure fails the turn, and the thread stays resumable. `reasoning_effort` carries the mapped effort. Assistant `reasoning_content` is stored as the replay payload and sent back on later turns, which DeepSeek thinking mode requires when tools are used. Usage maps prompt, completion, and total tokens plus cached prompt tokens and reasoning completion tokens; as with every adapter, input is the full prompt including cached tokens and total is input plus output. Default `base_url`: `https://api.openai.com/v1`; DeepSeek needs `https://api.deepseek.com`.

### `responses`

Streams `POST /responses` with `store: false` and `include: ["reasoning.encrypted_content"]`; the system prompt goes in `instructions`, tools are declared as function tools, and `reasoning` is `{effort, summary: "auto"}`. The replay payload is the turn's full output-item array (reasoning with encrypted content, message, function calls), replayed verbatim, with each tool result sent as a `function_call_output`. Text deltas stream from `response.output_text.delta`, the final state is taken from `response.completed`, and an `incomplete` or `failed` status fails the turn with the reason; a completed response whose message part is a `refusal` fails the turn with the refusal text instead of an empty answer. Rollout reasoning text is the concatenated reasoning summary. Usage maps input (the full prompt, cached tokens included), cache reads to cached as a subset of input, output, reasoning output to reasoning, and total, which is input plus output. Default `base_url`: `https://api.openai.com/v1`.

### `messages`

Streams `POST /v1/messages` with adaptive summarized thinking, `output_config.effort` set to the mapped effort, `max_tokens` from `max_output_tokens`, top-level ephemeral `cache_control` for automatic prompt caching, the system prompt in `system`, and tools declared with `eager_input_streaming`. The replay payload is the assistant turn's full content-block array (signed thinking, text, tool_use), replayed verbatim; all tool results of one turn go into one user message with one `tool_result` block per call and `is_error: true` when execution failed or was denied. A truncated or invalid tool input JSON yields an `is_error` result without running the tool. Stop reasons: `refusal` fails the turn including the category, `max_tokens` fails it suggesting a larger `max_output_tokens`, `model_context_window_exceeded` fails it suggesting a shorter conversation or a model with a larger context window, `pause_turn` replays the assistant content and continues within the same turn (at most 5 continuations), and `tool_use` and `end_turn` are normal. A stream that ends before its terminal stop signal fails the turn, carrying the partial text it had streamed; nothing is stored for replay. An empty `end_turn` reply (`content: []`) is accepted as an empty answer but is never replayed, so the session stays usable. Rollout reasoning text is the concatenated summarized thinking. Usage maps input tokens plus cache creation and cache reads to input, cache reads to cached as a subset of input, output tokens to output, and thinking tokens to reasoning; total is input plus output. Default `base_url`: `https://api.anthropic.com`.

## Cancellation and concurrency

The server honors the standard MCP `notifications/cancelled` message for in-flight `subagent` and `subagent-reply` calls. Cancelling a call stops the model request, kills any running shell command, and releases the thread lock; the call returns an error containing `context canceled`, and the thread stays resumable with `subagent-reply`. For `codex-app-server` sessions the lock is released only once the pending turn interrupt finishes, which can take up to 30 seconds after the cancellation, so a `subagent-reply` sent in that window gets a `busy` error instead of continuing the thread. A cancellation that arrives before a queued call starts is ignored.

The stdio server processes up to 32 tool calls concurrently (mcp-go's default is 5).

## Sandbox and approvals

The sandbox classifies operations as inside or outside its boundary. The approval policy then decides whether the operation runs, asks the MCP client for approval, or is denied.

| Sandbox | `untrusted` | `on-request` | `on-failure` | `never` |
|---|---|---|---|---|
| `read-only` | Allow `read_file` and allowlisted shell; ask for everything else | Allow `read_file` and allowlisted shell; ask for everything else | Same as `on-request` | Allow `read_file` and allowlisted shell; deny everything else |
| `workspace-write` | Allow `read_file` and allowlisted shell; ask for other shell and all `write_file` calls | Allow `read_file`, all shell, and `write_file` inside `cwd`; ask for `write_file` outside `cwd` | Same as `on-request` | Allow `read_file`, all shell, and `write_file` inside `cwd`; deny `write_file` outside `cwd` |
| `danger-full-access` | Allow `read_file` and allowlisted shell; ask for all other operations | Allow all operations | Same as `on-request` | Allow all operations |

The shell allowlist covers `ls`, `cat`, `head`, `tail`, `rg`, `grep`, `find`, `pwd`, `wc`, `stat`, `which`, and `echo`, plus the Git subcommands `status`, `diff`, `log`, `show`, `branch`, `blame`, `rev-parse`, and `ls-files`. It parses shell words before making the decision, including single/double quotes, backslashes, adjacent quoted pieces, and separators outside quotes, so the decision is based on the argv that the shell will execute rather than on raw command text. Commands with unquoted variable or command expansion (`$` or backticks), process substitution, redirection or heredocs, unquoted brace/pathname expansion, a leading tilde expansion, assignments, unknown segments, or selected dangerous flags fall outside the allowlist. Auto-allowed Git commands also run with repository-configured external diffs, textconv and content filters, fsmonitor, pagers, signature verification, submodule recursion, and relevant environment/config overrides disabled; approved commands outside the allowlist are not treated as safe by this policy. Approval elicitation failure or an unanswered request after five minutes is treated as denial.

`apply_patch` is evaluated like `write_file` once per touched path (including `*** Move to:` targets). Any denied path denies the whole patch, so a single patch cannot write inside `cwd` and outside it at the same time; every path that needs approval is listed in one combined approval request, and that approval covers the whole patch.

### Kernel sandbox for shell calls

Shell calls that the policy auto-allows run under a Landlock ruleset: the server re-executes its own binary as `subagent-mcp __sandbox-exec --cwd-fd <fd> --rw-fd <fd>... -- bash -lc <command>`. The server opens the cwd and writable roots when the session is created and passes those directory handles to the helper. The helper enters the held cwd and grants write access to those same directory inodes, so renaming a root or replacing its old path cannot grant access to a different directory. The existing `--rw <path>` helper form remains available for path-based callers. The ruleset permits read and execute everywhere and writes only beneath the held roots. The login profile is loaded before the command environment cleanup described above, so profile-provided `PATH` entries remain usable without re-exported secrets reaching the command. The restriction is inherited by every descendant process, so writes are blocked by the kernel rather than by command-string inspection.

| Sandbox | Writable roots for auto-allowed shell calls |
|---|---|
| `read-only` | None |
| `workspace-write` | `cwd`, `/tmp`, `$TMPDIR` when set and different and an existing directory, plus `config.writable_roots` |
| `danger-full-access` | Unrestricted; the command runs without the helper |

In both wrapped modes the device files `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`, and `/dev/tty` are also writable; the rest of `/dev`, including `/dev/shm`, is read-only. On Landlock ABI v2+ kernels (Linux 5.19+), files can be renamed or hard-linked between directories inside the writable roots.

For auto-allowed `write_file` and `apply_patch` calls, the in-process path check uses the session's bound cwd. If the original cwd path no longer names that directory, the write fails closed; a replacement symlink cannot redirect it. Shell calls that a human approved through elicitation run without the kernel sandbox, matching Codex escalation semantics. The sandbox fails closed: on a kernel without Landlock (Linux below 5.13) or on a non-Linux platform, a wrapped shell call exits with code 126 and `subagent-mcp: landlock unavailable: ...` instead of running unsandboxed.

**Safety: the application-layer policy prevents accidental misuse, and the Landlock wrapper confines auto-allowed shell writes to the roots above. Reads and network access are still unrestricted, and `write_file` and `apply_patch` writes are limited to `cwd` by an in-process path check rather than by the kernel. On Landlock ABI v1 kernels (Linux 5.13–5.18), renaming or hard-linking a file into a different directory always fails with `EXDEV` (tools such as `git mv` break, while `mv` falls back to copying). Truncation is only restricted from Landlock ABI v3 (Linux 6.2): on older kernels, truncating existing files outside the writable roots is not restricted. Human-approved and `danger-full-access` shell calls are not sandboxed at all. Use stronger operating-system isolation when the trust boundary requires it.**

## Events

During a running call, the server emits `subagent/event` notifications with `threadId` and a `msg` object. The possible `msg.type` values are:

| Type | Meaning and fields |
|---|---|
| `task_started` | Processing began. |
| `agent_message_delta` | Streamed assistant text in `delta`. |
| `token_count` | Model-turn usage in `prompt_tokens` (the full prompt, cached tokens included), `completion_tokens`, and `total_tokens` (their sum). |
| `exec_command_begin` | Tool execution began; includes `call_id`, `tool`, and `command` for shell, `paths` (array) for `apply_patch`, or `path` for `read_file` and `write_file`. |
| `exec_command_end` | Tool execution ended; includes `call_id`, `tool`, `exit_code` for shell, `paths` for `apply_patch`, and `error` when execution failed. |
| `agent_message` | Final assistant text in `message`. |
| `task_complete` | The call completed successfully. |
| `error` | The call failed; `message` contains the error. |

Operations stopped by policy or denied approval do not begin execution and therefore do not emit `exec_command_begin` or `exec_command_end`. For `codex-app-server` sessions Codex reports `item/started` before it requests approval, so a file change that is then declined has already emitted `exec_command_begin` and still emits `exec_command_end`; those events show what Codex reported, not whether the approval was granted.

When a `tools/call` carries `_meta.progressToken`, the server additionally sends standard `notifications/progress` notifications for that call alongside the unchanged `subagent/event` notifications. The `progress` value increases from 1, and `message` carries a short summary: `started`, `shell: <cmd>`, `apply_patch: <paths>`, `<tool>: <path>`, `agent: <first line>`, `completed`, or `error: <msg>`, each at most 200 characters.

## Rollout files

Every session writes a Codex-compatible rollout as JSONL under `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<threadId>.jsonl`, using UTC and `~/.codex` when `CODEX_HOME` is unset. Directories are created `0700` and the file is `0600`. Each line is `{"timestamp": ..., "type": ..., "payload": {...}}` with the Codex line types `session_meta`, `turn_context`, `response_item`, and `event_msg`. Codex ecosystem tools can read these files for usage reporting, session viewing, and audit; `originator` is `subagent-mcp`, `model_provider` is the selected provider's config name, `turn_context` records both the caller's `effort` and the mapped `effort_sent`, and `reasoning` items come from the provider's human-readable reasoning text and are omitted when empty. `session_meta` records the working directory, the full composed system prompt, and the Git branch and commit when `cwd` is in a repository. Replay payloads are never written. Because the files are written but never read back, these sessions are visible to `codex resume` but cannot actually be resumed by Codex.

Set `SUBAGENT_MCP_ROLLOUT=off` to disable rollout writing. A rollout write or open failure logs one line to stderr and disables the rollout for that session; it never fails an agent run.

The rollout records the prompt, the composed system prompt, tool arguments, tool results, and command output, so it can contain secrets and file contents. Treat the rollout directory with the same care as the session transcript.

## Notes

- This repository's `.mcp.json` exposes `subagent` as a project-level MCP server when Claude Code is opened inside the repository, which is useful for self-testing.
- Sessions are stored in memory only. They are lost when the server restarts; there is no persistence or cross-process resume, and rollout files are write-only. Sessions idle for more than 24 hours are evicted, and when more than 256 sessions exist the least recently used idle ones are evicted; busy sessions are never evicted. An evicted `threadId` returns `unknown threadId` like a server restart.
- Run `go test ./... -race` to execute the test suite with the race detector.

## Non-goals

- Switching provider mid-thread or per call.
- Third-party hosting of Anthropic or OpenAI models (Bedrock, Vertex, Foundry, Azure OpenAI).
- Image input, provider server-side tools (web search, code execution), and context compaction.
- Network or read sandboxing, and sandboxing on non-Linux platforms.
- Persistent or restart-resumable sessions, and backward compatibility with the pre-0.6.0 names, environment variables, or file-based credential handling.
- MCP resources or prompts. The Claude Code plugin also deliberately omits agents, hooks, and background broker scripts.
