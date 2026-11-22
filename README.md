# subagent-mcp

An MCP server that runs coding-agent sessions with shell and file tools. Choose a configured provider/model, let a decision model choose once per session, or delegate the session to Codex app-server. Install it as a Claude Code or Codex plugin, or register the binary with any MCP client.

| `api` | Protocol | Example providers |
|---|---|---|
| `chat-completions` | OpenAI Chat Completions | DeepSeek, OpenAI-compatible endpoints |
| `responses` | OpenAI Responses API | OpenAI |
| `messages` | Anthropic Messages API | Anthropic |
| `codex-app-server` | Codex app-server over stdio | Local Codex CLI (experimental) |

Native providers use the built-in tools, approvals, Linux Landlock write sandbox, progress notifications, and session recording. Codex uses its own tools, sandbox, and login.

## Install

### Claude Code plugin

On Linux or macOS, install the plugin:

```bash
claude plugin marketplace add Geek0x0/subagent-mcp
claude plugin install subagent@subagent-mcp
```

For a local checkout, replace `Geek0x0/subagent-mcp` with `/path/to/subagent-mcp` in the marketplace command.

The launcher downloads the matching release binary on first start, verifies it against `scripts/release.lock`, and caches it in the plugin's data directory. Set `SUBAGENT_MCP_BIN` to use an existing binary instead.

Create the config below, supply its API key in the environment that launches the MCP server, then restart or reconnect the server and run `/subagent:setup`.

### Codex plugin

On Linux or macOS, install the plugin into Codex:

```bash
codex plugin marketplace add Geek0x0/subagent-mcp
codex plugin add subagent@subagent-mcp
```

Codex uses the same launcher, the `subagent-dispatch` skill, and exposes the tools as `mcp__subagent__subagent` and `mcp__subagent__subagent_reply`. The launcher caches the binary under `~/.cache/subagent-mcp` (or `$XDG_CACHE_HOME`).

Codex starts MCP servers with a minimal environment and forwards only the variables listed in [`.codex-mcp.json`](https://github.com/Geek0x0/subagent-mcp/blob/main/.codex-mcp.json): the `SUBAGENT_MCP_*` variables, `DEEPSEEK_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `TYPESAFE_API_KEY`, `CLOUDFLARE_API_TOKEN`, `CODEX_HOME`, proxy settings, and a few path variables. A provider whose `env_key` uses another name will not see its key; use one of these names or a `key` in the config file. The `/worker` policy hooks and the `worker-sonnet` agent are Claude Code only. Codex installs a local marketplace by copying the whole directory, including `target/`, so prefer the GitHub source over a local checkout.

Create the config below, export its API key before starting Codex, then start a new Codex session and ask it to check the subagent-mcp installation. That uses the plugin's `subagent-setup` skill, the Codex counterpart of `/subagent:setup`: it runs `--check-config` against the cached binary (downloading it first if needed) and also reports whether Codex forwards each provider's key to the server.

### Manual installation

For Windows or direct MCP registration, download a [release](https://github.com/Geek0x0/subagent-mcp/releases) for Linux, macOS, or Windows (amd64/arm64), and put `subagent-mcp` on `PATH`. Archives include this README and `config.example.toml`. Windows requires `bash` on `PATH`, for example Git Bash.

To build from source with Rust 1.94.1:

```bash
cargo build --release --locked
```

The binary is `target/release/subagent-mcp`. See [development and releases](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/development.md) for tests and packaging.

**Platform support:** Linux is the supported platform. Auto-allowed shell calls in `read-only` or `workspace-write` require Landlock (Linux 5.13+); on macOS, Windows, or an unavailable Landlock kernel they fail closed with exit code 126. Native file tools still work. `--check-config` reports Landlock availability. See [sandbox limitations](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/sandbox.md).

## Configuration

Create `~/.config/subagent-mcp/config.toml` with one provider to get started:

```toml
[providers.deepseek]
api = "chat-completions"
base_url = "https://api.deepseek.com"
env_key = "DEEPSEEK_API_KEY"
default_model = "deepseek-flash"
models = [{ id = "deepseek-flash" }]
```

Set `DEEPSEEK_API_KEY` in the MCP server's launching environment, such as the client's `env` block. Agent shell commands have configured key variables removed, including values re-exported by login profiles.

For multiple providers, OpenAI, Anthropic, Codex, and optional auto selection, copy and edit [config.example.toml](https://github.com/Geek0x0/subagent-mcp/blob/main/config.example.toml). The config is loaded once at startup; restart or reconnect after changes. `SUBAGENT_MCP_CONFIG` overrides its path. Missing files, unknown keys, and invalid fields fail with a path or field name.

A provider may use `key` instead of `env_key`, but never both. A config containing a key must have mode `600` (not checked on Windows). Shell tools can still read keys stored in files; see [API key handling](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/configuration.md#api-keys).

### Validating the config

```bash
subagent-mcp --check-config
subagent-mcp --check-config /path/to/config.toml
```

This validates the config, reports sandbox availability, and checks authentication and model lists for reachable providers. `PASS` means at least one provider was checked and none failed. Missing model IDs or unavailable Landlock produce warnings; provider check failures or no usable provider produce a nonzero exit code. No key values are printed.

`subagent-mcp --check-config --live` additionally makes a real, billed test call per reachable provider. Use it deliberately. See [validation details](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/configuration.md#validating-the-config) for output examples and Codex checks.

## Tools

The server exposes `subagent` to start a session and `subagent-reply` to continue it. Claude Code displays these as `mcp__subagent__subagent` and `mcp__subagent__subagent-reply` under the default server name.

Start a read-only session with a JSON tool argument object:

```json
{
  "prompt": "Read this repository and summarize its main components.",
  "provider": "deepseek",
  "cwd": "/absolute/path/to/project"
}
```

| Argument | Default | Common use |
|---|---|---|
| `prompt` | Required | Task for the agent. |
| `provider` | Sole configured provider | Required with multiple providers and an explicit model. |
| `model` | Provider's `default_model` | A configured model ID, or `auto` with no `provider`. |
| `cwd` | Server working directory | Absolute path to an existing directory. |
| `sandbox` | `read-only` | Use `workspace-write` for edits; `danger-full-access` removes shell confinement. |
| `approval-policy` | `on-request` | Operations outside policy require client approval. |
| `reasoning-effort` | `high` | Mapped through the provider's optional `effort_map`. |

Retain `structuredContent.threadId` from the result and pass it to `subagent-reply`:

```json
{
  "threadId": "<threadId from the start result>",
  "prompt": "Explain the main entry point in more detail."
}
```

The response identifies the actual `provider` and `model`. Session settings stay fixed, and only one call may run per thread; another returns `busy`. Sessions live in memory and cannot be recovered after a server restart. Idle sessions expire after 24 hours; the server also evicts idle sessions when more than 256 exist.

Native sessions have four tools: `shell`, `read_file`, `write_file`, and `apply_patch`. See the [tool reference](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/tools.md) for all arguments, budgets, output limits, and Codex-compatible MCP tool names.

### Auto model selection

Configure `[auto]`, then pass `model: "auto"` and omit `provider`. The decision model selects one reachable provider/model at session creation; replies keep that choice. A single candidate skips the decision call. Decision errors fail the call without a fallback.

Auto mode sends the task prompt and validated working directory to the configured decision endpoint (TypeSafe by default). See [auto configuration and Clef support](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/configuration.md#auto-model-selection).

### Codex app-server provider

Enable the Codex block in `config.example.toml`, install Codex, and run `codex login` as the user launching the server. Model IDs must match that Codex installation; `--check-config` reports missing IDs. This provider is experimental and inherits the user's Codex config, plugins, hooks, skills, MCP servers, and AGENTS.md.

Codex owns its tools, sandbox, and rollout. Shell, file-change, and network approvals are relayed; other interactive requests are unsupported. See [Codex setup and differences](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/configuration.md#codex-app-server-provider).

## Worker policy

The Claude Code plugin provides `/worker on|off` and `/worker mode mcp|sonnet`. When delegation is off or Sonnet mode is selected, the plugin refuses calls to its MCP worker tools. This needs a Claude Code build supporting plugin hooks modules; the Codex plugin does not include the worker policy. See [worker policy details](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/runtime.md#worker-policy).

## Sandbox and approvals

For native providers, the sandbox sets the operation boundary and the approval policy decides whether to allow, ask, or deny. In `workspace-write`, auto-allowed shell writes are confined to `cwd`, temporary directories, and configured writable roots. Native file writes are limited to `cwd`. Server-side limits can cap requested access and allowed directories.

**Reads and network access are unrestricted.** Human-approved shell calls and `danger-full-access` shell calls run without Landlock. Kernel-version restrictions, reachable local sockets, and deliberately detached processes limit confinement; use stronger operating-system isolation where required. See the [approval matrix and full sandbox limitations](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/sandbox.md).

Calls support MCP cancellation; ordinary shell children are killed, and the thread remains usable. Client disconnection cancels calls and shuts down the server. See [cancellation, concurrency, and shutdown](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/runtime.md#cancellation-and-concurrency).

## Rollout files

Native sessions record Codex-compatible JSONL under `$CODEX_HOME/sessions` (default `~/.codex/sessions`). Set `SUBAGENT_MCP_ROLLOUT=off` to disable recording, or configure `[rollout] format = "opencode"` to record in OpenCode instead. Codex-provider sessions use Codex's own recording; this switch does not disable it.

**Transcripts can contain secrets, file contents, and command output.** OpenCode API recording exposes the transcript temporarily in process arguments; database mode writes another application's database, and each run replaces its session copy. Read the [rollout and OpenCode reference](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/rollouts.md) before enabling that integration. Native rollout files are write-only and cannot restore sessions through `codex resume`.

## Reference

| Document | Contents |
|---|---|
| [Configuration](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/configuration.md) | All fields, keys, effort, gateways, nudges, server limits, validation, and environment variables. |
| [Tools](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/tools.md) | Start/reply contracts, budgets, prompt loading, native tools, and MCP naming. |
| [Runtime](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/runtime.md) | Worker policy, provider protocols, retries, cancellation, concurrency, and shutdown. |
| [Sandbox](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/sandbox.md) | Approval matrix, allowlist, Landlock behavior, and security boundaries. |
| [Events](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/events.md) | `subagent/event` fields and standard MCP progress notifications. |
| [Rollouts](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/reference/rollouts.md) | Codex JSONL and OpenCode recording, failure handling, and privacy. |
| [Development](https://github.com/Geek0x0/subagent-mcp/blob/main/docs/development.md) | Build, tests, packaging, and plugin release pins. |

## Non-goals

- Switching provider mid-thread or selecting a new model on every auto-mode turn.
- Third-party hosting of Anthropic or OpenAI models (Bedrock, Vertex, Foundry, Azure OpenAI).
- Image input, provider server-side tools (web search, code execution), or context compaction.
- Network or read sandboxing, or native shell sandboxing on non-Linux platforms.
- Persistent or restart-resumable sessions; compatibility with pre-0.6.0 names, variables, or file-based credential handling.
- MCP resources or prompts; background broker scripts in the Claude Code plugin.
