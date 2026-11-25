# Configuration reference

The TOML file is loaded once at startup from `$SUBAGENT_MCP_CONFIG`, or from `~/.config/subagent-mcp/config.toml` when that variable is unset. A missing file fails startup with a message naming the path and pointing at [`config.example.toml`](../../config.example.toml); the repository's [`config.example.toml`](../../config.example.toml) has a ready-to-edit deepseek/openai/anthropic setup. Decoding is strict, so a typo is an error rather than a silently ignored key, and every validation error names the offending field path (for example `providers.openai.default_model`).

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

# Optional: let `start` choose a provider/model with TypeSafe's Jev (or Clef below).
[auto]
env_key = "TYPESAFE_API_KEY"
model = "jev-latest"
# base_url = "https://api.typesafe.ai"
# timeout_seconds = 20
```

## Fields

| Field | Level | Required | Rules |
|---|---|---|---|
| `api` | provider | yes | One of `chat-completions`, `responses`, `messages`, `codex-app-server`, `acp` (experimental; see [ACP agents](#acp-agents-experimental)). |
| `base_url` | provider | no | Defaults per api: `https://api.openai.com/v1` for `chat-completions` and `responses`, `https://api.anthropic.com` for `messages`. Rejected when non-empty for `codex-app-server`, which has no URL default. |
| `env_key` | provider | one of `env_key`, `key`; except Codex | Name of the environment variable holding the key. Its value is read when a session is created for that provider (via the `provider` argument, or the sole configured provider); a missing value fails that call naming the variable and `providers.<name>.env_key`. Rejected when non-empty for `codex-app-server`: authentication goes through `codex login`. |
| `key` | provider | one of `env_key`, `key`; except Codex | The API key itself, instead of `env_key`; setting both is an error. A config file that holds a key must be mode `600` (see [API keys](#api-keys)). Rejected when non-empty for `codex-app-server`. |
| `command` | provider | for `acp` | Executable name or path. For `codex-app-server` it is optional, defaults to `codex` on `PATH`, and the server adds the `app-server` argument. For `acp` it is required and nothing is appended; pass arguments with `args`. This is not a shell command or argument list. Rejected when non-empty for every other api. |
| `args` | provider | no | `acp` only. List of arguments passed to `command` as given, with no shell. Rejected for every other api. |
| `env` | provider | no | `acp` only. Table of environment variables added to the agent's environment after the server's scrubbing (`[server] scrub_env`, provider `env_key` variables and `SUBAGENT_MCP_*`). Rejected for every other api. |
| `default_model` | provider | yes | Must be one of `models[].id`. Used when a caller omits `model`. |
| `models` | provider | yes | Non-empty list of `{ id, description? }` tables; ids must be unique and are advertised in config order. |
| `effort_map` | provider | no | Maps a caller effort value to the value sent to the API: `effort_map[value]` if present, else the caller's value. Keys and values must be in the effort set below. |
| `max_output_tokens` | provider | no | Positive integer output limit. `messages` always sends one as `max_tokens` and defaults to `64000`, because that API requires the field. `responses` sends it as `max_output_tokens` and `chat-completions` as `max_tokens` when the config sets it; leaving it unset omits the field, so the provider's own default applies. OpenAI's reasoning models reject `max_tokens`, so leave this unset for one on the `chat-completions` api and set `extra_body.max_completion_tokens` instead, or use the `responses` api, which takes `max_output_tokens`. Rejected for `codex-app-server`, which applies its own limits. |
| `extra_body` | provider | no | Table merged into the JSON body of every `chat-completions` request, for fields the SDK has no place for. Its main use is choosing the upstream behind an OpenRouter-style gateway, for example `[providers.x.extra_body.provider]` with `ignore = ["relace"]` or `only = ["streamlake"]`. Rejected for other `api` values; `model`, `messages`, `stream`, `stream_options`, `tools`, `tool_choice` and `max_tokens` cannot be set (the last one is what `max_output_tokens` sends). `max_completion_tokens` stays available, for a model that requires it. See [Choosing the upstream behind a gateway](#choosing-the-upstream-behind-a-gateway). |
| `max_nudges` | provider | no | Integer from 0 to 10; defaults to `0` (off). See [Models that stop to narrate](#models-that-stop-to-narrate). Ignored by `codex-app-server`. |

The optional `[auto]` table configures the decision model used by `model: "auto"`:

| Field | Required | Rules |
|---|---|---|
| `env_key` | one of `env_key`, `key` | Name of the environment variable holding the decision model API key. A missing value fails only calls that need a decision, naming the variable and `auto.env_key`. |
| `key` | one of `env_key`, `key` | The decision model API key itself, instead of `env_key`; setting both is an error. Same file-mode rule as a provider `key`. |
| `model` | yes | Decision model name or alias, for example `jev-latest`, `clef`, or `clef-flash`. |
| `base_url` | no | Defaults to `https://api.typesafe.ai`. The System One path `/v1/systemone` is appended. A same-protocol proxy can be used by pointing here. Mutually exclusive with `url`. |
| `url` | no | Full decision endpoint used verbatim; mutually exclusive with `base_url`. |
| `timeout_seconds` | no | Bound on one decision request; `0` (or omitted) uses the default `20`, otherwise valid values are 1 to 600. The caller's own deadline still wins when shorter. |

`auto` is a reserved pseudo-model id: a `providers.*.models[].id` equal to `auto` is a config error, whether or not `[auto]` is configured. Without `[auto]`, `model: "auto"` is a tool error naming the missing section.

## Choosing the upstream behind a gateway

Gateways such as OpenRouter (Cline Pass uses one) serve one model from many upstream channels and may pick a different one for each call. An upstream that fails to stream a tool call produces replies that arrive empty or end after a sentence of narration, even though the model generated tokens. Set `extra_body` on the provider to steer routing:

```toml
[providers.cline-glm-flash.extra_body.provider]
ignore = ["relace"]          # never use this upstream
# only = ["streamlake"]      # or: use only these
# order = ["streamlake", "deepinfra"]
```

Names are the gateway's lowercase slugs. An unknown name in `only` is refused with an error that lists the valid ones; asking for a model with `only = ["__probe__"]` is a quick way to see that list. A 200 response does not prove a preference was applied, so check which upstream served a call: when the API names it (the `provider` field of OpenRouter-style streams), the `provider_request` event carries it as `upstream`. Gateways that route through Vercel AI Gateway ignore `provider` and read `providerOptions.gateway.only` instead; both can be set in the same table.

## Models that stop to narrate

The agent loop treats a model reply without a tool call as the final answer. Some small models end a turn that way while work remains ("Now step 2: write the failing test"), so the call returns that sentence as its result. With `max_nudges` above zero, such a reply is answered with a "continue" prompt (an `agent_nudge` event is emitted) and the loop goes on: a model that was really finished repeats its summary, and a second reply without a tool call right after the prompt is accepted as final. Each stretch without tool calls is nudged at most once, and at most `max_nudges` times per run. Empty replies count as replies without a tool call, are not replayed to the API, and (being never a final answer) can be nudged again until the budget is spent, with a prompt that says the last reply was empty and asks for a smaller tool call (an API can drop a tool call it fails to stream, and the same call tends to be lost the same way again). The cost of the option is one extra model call at the end of a run that finishes normally. Prompting alone did not fix it in testing, so this is a code-level guard; set it on the providers that need it, for example `max_nudges = 3`.

## Reasoning effort

Accepted caller values are `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, and `ultra`; the default is `high`. The value sent to the provider is `effort_map[value]` when the selected provider defines that mapping, otherwise the caller's value. The server validates against the eight values above and does not otherwise check provider-specific support, so an unsupported value surfaces as the provider's API error. `ultra` is the level Codex describes as maximum reasoning with automatic task delegation; the three model API families have no equivalent, so leave it unset for them or map it with `effort_map`. `config.model_reasoning_effort` in the `config` object works like `reasoning-effort`, and the top-level argument wins.

## API keys

A provider names its key in one of two ways: `env_key`, the name of an environment variable, or `key`, the key itself in the config file.

With `key`, every provider's settings live in the one file and nothing has to be exported into the environment that launches the server. The file then holds a secret, so the server refuses to start unless its mode is `600` (`chmod 600 ~/.config/subagent-mcp/config.toml`; not checked on Windows), and neither `--check-config` nor any error message prints the value. Be aware of what this does not protect: the agent's `read_file` tool refuses the config file, but its `shell` tool can read any file the server's user can, so a command such as `cat` on the config path returns the keys to the model and into the rollout file. The same is true of a key kept in an MCP client's `env` block on disk. Prefer `env_key` with a variable that exists only in the launching process when that matters.

With `env_key`, for the three model API families, when a session is created, the server reads the selected provider's `env_key` variable from its own environment; every provider's `env_key` variable is also removed from shell commands run by the agent (see [Environment](#environment)). `auto.env_key` is scrubbed the same way. Keep the variable in the environment that launches the MCP server, for example in an MCP client's `env` block. Do not rely on a shell profile to provide the key to the agent: login profiles are loaded for shell commands, but configured keys are removed again before the command runs. Codex uses its own login instead.
Requests to a provider's `base_url` carry only that provider's configured key and the SDK's standard protocol headers: the SDKs' own credential environments (`ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_CUSTOM_HEADERS`, `OPENAI_ORG_ID`, `OPENAI_PROJECT_ID`, `OPENAI_CUSTOM_HEADERS`, and the `*_BASE_URL`/`*_API_KEY` variables) are ignored, so gateway tokens and headers from the server's environment are never forwarded to a configured endpoint.

## Server-side limits

A `[server]` section caps what one `start` call may ask for, so a compromised or confused orchestrator cannot hand itself more access than the operator intended. Every key is optional.

```toml
[server]
max_sandbox = "workspace-write"          # read-only | workspace-write | danger-full-access
max_approval_policy = "on-request"       # untrusted | on-request | on-failure | never
allowed_cwd_roots = ["/srv/work"]        # absolute paths; when set, cwd must be under one
scrub_env = ["GITHUB_TOKEN", "AWS_*"]    # extra names/prefixes hidden from shell and Codex children
```

A requested `sandbox` or `approval-policy` that is more permissive than the ceiling is clamped to it, and the start result carries a `warning:` line saying so. `allowed_cwd_roots` is an allowlist: both the `cwd` and every `config.writable_roots` entry are resolved through symlinks and must land under one of the roots, and a `cwd` outside every root is a tool error before a session is created. `scrub_env` entries ending in `*` are prefixes and must keep a non-empty prefix, so a bare `*` is a config error instead of hiding the whole environment; `SUBAGENT_MCP_*` and every provider `env_key` are always removed.

## Selecting a provider

Pass `provider` when starting a session to choose which configured provider it uses; the tool schema's `enum` lists every name defined under `[providers]`. Omitting `provider` works only when the config defines exactly one provider — with two or more, omitting it is a tool error listing the available names. The chosen provider is fixed for the session's lifetime: `subagent-reply` always continues with the same one, and each call's `model` must come from that provider's list.

## Auto model selection

With an `[auto]` section configured, pass `model: "auto"` and omit `provider` to let the server choose the provider/model for that session. The server sends the task prompt and validated `cwd` to the configured decision model (TypeSafe Jev by default, using `POST /v1/systemone`) with one `choice` question whose criteria are every reachable configured provider/model pair and its configured `description`. Providers whose API key is not set are omitted; `codex-app-server` providers are included without a login preflight. The highest-probability choice is dispatched, and the start result reports it.

- Well-known model IDs get a built-in default description when none is configured; a configured description overrides it. An `[auto.descriptions]` table maps a model id (or the suffix of a prefixed id) to a one-line description, which is the supported way to keep the built-in table from going stale for a gateway's model names.
- A single reachable candidate skips the decision model entirely and does not require `auto.env_key`.
- A decision model error, malformed answer, or unknown choice is a tool error. There is no fallback model and no retry; retry the call or pass an explicit `provider`/`model`.
- `auto.confidence` is informational. A low-confidence choice is still dispatched, because it is the decision model's highest-probability answer.
- One decision happens at session creation. `subagent-reply` keeps the chosen provider/model for the session's lifetime.
- Auto mode sends the prompt to the configured endpoint (TypeSafe by default). The long-context header is TypeSafe-specific and allows long prompts billed by input tokens; Clef has its own 64K context window and truncates long state.
- The decision is a label, not generated text: the task prompt can bias which model is chosen, but it cannot make the decision model execute tools or return free-form instructions.

Cloudflare Clef is also supported as a Jev-compatible backend. Replace the default `[auto]` block with:

```toml
[auto]
url = "https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/run/@cf/cloudflare/clef"
env_key = "CLOUDFLARE_API_TOKEN"
model = "clef" # or "clef-flash"
```

## Codex app-server provider

`codex-app-server` is **experimental**, tested with **codex-cli 0.156.1**; its `auth` and `model/list` calls also work against 0.131.0 and 0.164.0. Install Codex, run `codex login` as the user launching the MCP server, and enable the commented `[providers.codex]` block in [`config.example.toml`](../../config.example.toml). Set `command` only if the executable is not `codex` on `PATH`; omit `env_key` and `base_url`.

The model ids a provider block lists are whatever the installed Codex reports, and they change between Codex versions: 0.131.0 lists only `gpt-5.x`, while 0.164.0 lists the `gpt-6.x` family and no `gpt-5.5`, so a block written against one version will not validate against the other. If more than one Codex is installed, point `command` at the one whose models you listed, since an older install earlier on `PATH` wins the plain `codex` lookup. `--check-config` reports the version and model count it found and warns about every configured id that is not in that list.

The session inherits the user's whole Codex environment — auth, config, plugins, hooks, skills, MCP servers, and AGENTS.md — and uses **Codex's own tools and sandbox**, not subagent-mcp's native four-tool runner or Landlock policy. The app-server child still receives an environment scrubbed of configured `env_key` variables and `SUBAGENT_MCP_*` variables. The native tool, prompt-loading, sandbox, and rollout details in the other reference documents apply to the three model API families; Codex differs as follows:

- `config.max_turns` is ignored; Codex manages the agent loop.
- AGENTS.md is loaded by Codex, not assembled into the prompt by subagent-mcp.
- `base-instructions` replaces Codex's base prompt only when given a non-empty value; omitting it leaves Codex's own base prompt intact. `developer-instructions` is forwarded separately.
- Codex owns the non-ephemeral thread and its rollout under `~/.codex/sessions` (or the user's configured Codex home). It appears in Codex history; subagent-mcp does not write a second rollout, and `SUBAGENT_MCP_ROLLOUT=off` does not disable Codex's rollout.
- `sandbox` is forwarded to Codex; `approval-policy` is forwarded with `on-failure` mapped to `on-request`. Command and file-change approvals are relayed to the MCP client. `config.writable_roots` is forwarded only for `workspace-write`.
- The relayed approval's target is the shell command, the touched paths for a file change, or — for a network approval — the requested host. Every other request Codex sends to the server (an MCP server's elicitation, a tool user-input request, a permission request, a dynamic tool call, or anything else) is answered automatically with the error `-32601 not supported by subagent-mcp`, so Codex's MCP servers and tools cannot prompt through subagent-mcp; those requests fail inside Codex instead of reaching your MCP client.
- A pending approval wait ends when the call is cancelled or the five-minute approval timeout expires (denial), and also when the Codex child process dies or the thread closes, so a pending approval never outlives either.
- Codex sends `item/started` before it asks for approval, so a declined file change still emits `exec_command_begin` and `exec_command_end`; see [Events](events.md).

## ACP agents (experimental)

`api = "acp"` runs any [Agent Client Protocol](https://agentclientprotocol.com) agent over stdio, protocol version 1. Each session spawns one child process from `command` and `args` in the session `cwd`; the child gets the scrubbed environment plus `env`. Like Codex, the agent uses its own tools, prompt and loop, not subagent-mcp's native runner. `base_url`, `env_key`, `key`, `max_output_tokens` and `extra_body` are rejected for `acp`; `max_nudges` is accepted and ignored.

```toml
[providers.claude]
api = "acp"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]
env = { SOME_FLAG = "1" }
default_model = "default"
models = [{ id = "default", description = "whatever the agent picks" }]
```

Limits:

- **Login happens in the agent itself.** subagent-mcp sends no key and never calls `authenticate`. `--check-config` reports the agent's `agentInfo` and advertised `authMethods`; a session that fails with "authentication required" means you must log in with the agent's own tooling.
- **Do not put credentials in `env`.** `env` values sit in the config file in plain text, and the mode `600` check covers only `key`. Prefer letting the agent use its own login, or a variable in the environment that launches the server.
- **Instructions.** When `initialize` advertises `systemPrompt` support (for example `agentCapabilities.sessionCapabilities.systemPrompt`), the base and developer instructions are sent as `systemPrompt` in `session/new`. Otherwise they are prefixed to the first prompt inside `<instructions>` tags.
- **Sandbox is advisory.** `sandbox`, `max_sandbox` and `config.writable_roots` are not enforced by subagent-mcp; the agent runs with the server user's permissions and its own policy, if any.
- **The read-only auto-reject applies only to agents that request permission.** Under `read-only`, a `session/request_permission` for an `edit`, `delete`, `move` or `execute` tool call is rejected without asking the MCP client. An agent that acts without asking is not stopped.
- **Approval policy has no effect.** `approval-policy` (and the server's `max_approval_policy`) is ignored for ACP. Every `session/request_permission` is answered by asking the MCP client, except for the read-only auto-reject above. When to ask is decided by the agent.
- **No resume.** Each thread is a fresh agent session; `ephemeral` has no effect.
- **No respawn.** If the agent process dies, the thread fails; start a new session.
- **Effort is ignored.** `reasoning-effort` and `effort_map` have no ACP equivalent; a requested effort is ignored with one warning per thread. A model other than `default` (or empty) is applied after `session/new`. If the `session/new` result lists a `configOptions` entry with `category` or `id` `model`, the model must be one of its listed values (when it lists any) and is set with `session/set_config_option`. Otherwise the legacy `session/set_model` is used. If the requested model cannot be applied (there is no mechanism, the method is not found, the params are invalid, or the value is not listed), `start` fails with an error naming the model; it does not warn and continue.
- **Process teardown.** On Unix the agent is started in its own process group and the whole group is killed on close or shutdown. On Windows only the direct child process is killed, so a wrapper agent (for example one launched through `npx`) can leave orphaned grandchildren.

## Validating the config

`--check-config` loads and validates the config file, reports whether the Landlock kernel sandbox is usable on this machine (a `WARN` there does not change the exit code), reports each model API provider's `env_key` name and whether it is set (or Codex's `auth` status, or an ACP agent's `agentInfo` and `authMethods` from `initialize`), and asks each reachable provider for its model list so authentication and reachability errors fail loudly and unlisted model ids are warned about:

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
auto     decision model (jev-latest)
  api    https://api.typesafe.ai
  key    TYPESAFE_API_KEY   set
result   PASS (1 checked, 1 skipped, 0 failed)
```

When `[auto]` exists, `--check-config` adds the `auto` block shown above and reports whether its key is set; it does not call the decision model. A missing auto key warns (`not set, decision model selection calls will fail`); it only affects auto calls with multiple reachable candidates, because a single reachable candidate skips the decision model. The warning does not change the exit code.

If no configured provider has a key set or passes Codex auth, and none failed, the result line instead reads `result   FAIL (0 checked, N skipped, 0 failed) — no configured provider has its key set`.

For a Codex-only config, an illustrative `--check-config` result is:

```text
config   /home/you/.config/subagent-mcp/config.toml   OK
provider codex (codex-app-server)
  auth   chatgpt (plus)   OK
  api    codex-cli 0.164.0   OK (8 models listed)
  model  gpt-6.1-sol     OK
  model  gpt-6-astra     OK
  model  gpt-6.1-terra     OK
result   PASS (1 checked, 0 skipped, 0 failed)
```

The `auth` line replaces `key`: the server starts Codex app-server and calls `account/read`, then `model/list` for the `api` and `model` lines. Account type, plan, version, and model count depend on the installation. A missing login prints `  auth   FAIL: not logged in; run codex login`; a process-start error also prints `  auth   FAIL: <error>`. A model-list failure prints `  api    FAIL: <error>`. Passing auth counts as checked; an auth or API failure counts as failed.

With `[rollout] format = "opencode"` the report also prints a `rollout  opencode   mode <mode>, fallback <fallback>` line followed by `bin` and `db` lines saying whether the `opencode` binary and its database were found; this never counts towards the result. Nothing is printed for the default format.

`model ... WARN` means a configured model id is not in the provider's current list; it does not affect the exit code. `--live` is opt-in and makes one real, billed tool-call round trip per reachable model API provider using `default_model` and effort `low`. For Codex it instead runs one ephemeral, read-only thread with approval policy `never`, asking for the sum of two numbers — the total never appears in the prompt — and checking that the final answer contains that total, so a reply that merely quotes the prompt fails. Its line is `  live   <default_model>   OK (turn succeeded, <duration>)` or `  live   <default_model>   FAIL: <error>`. Both paths print the billed-call warning first; use `--live` deliberately, not in CI. Neither mode ever prints a key value.

## Environment

| Variable | Description |
|---|---|
| `SUBAGENT_MCP_CONFIG` | Config file path; defaults to `~/.config/subagent-mcp/config.toml`. |
| `SUBAGENT_MCP_TOOL_NAME` | Base name for the two MCP tools; defaults to `subagent`, exposing `subagent` and `subagent-reply`. `codex` registers `codex` and `codex-reply`. The value must match `^[A-Za-z0-9_-]{1,64}$` and must not end with `-reply`; an invalid value fails startup naming `SUBAGENT_MCP_TOOL_NAME`. |
| `SUBAGENT_MCP_ROLLOUT` | Set to `off` to disable rollout files. |
| `SUBAGENT_MCP_ROLLOUT_FORMAT` | `codex` (default) or `opencode`: which rollout format a session is recorded in; overrides `[rollout] format`. See [OpenCode format](rollouts.md#opencode-format). |
| `SUBAGENT_MCP_DEBUG_STREAM` | Directory (created `0700`) that receives the raw response of every model API request, one `0600` file per request named `<UTC time>-<api>-<n>.sse`: a `#` header with the method, URL without query, status and content type, then the body exactly as received. For diagnosing what a server really streamed (for example a reply the parsed result shows as empty). Request headers and bodies are never written, so neither the key nor the prompt reaches the files, but the model's output does; unset it when done. Not applied to `codex-app-server`. |

Before running any shell command, the server removes from the command environment every variable named by any provider's `env_key` and every variable whose name starts with `SUBAGENT_MCP_`, including values a login profile tries to re-export. The command still runs through `bash -lc`, so profile-provided settings such as `PATH` work; cleanup happens after the profile is loaded. On Linux, the server also starts non-dumpable, preventing a same-user shell child from reading the server's environment through `/proc`. `read_file` additionally refuses to read the loaded config file, resolved through symlinks, and the server's own `/proc/<pid>` entries (`environ`, `mem`, `cmdline`, ...), to keep provider topology and keys out of model context. Reads are otherwise unrestricted. In particular the server being non-dumpable protects only its own `/proc/<pid>/environ`: a shell command can still read the environment of an ancestor process such as the MCP client that launched the server (`cat /proc/$PPID/environ`), and any key stored in a file. Deliver keys through the MCP client's `env` block or a file with restricted access, not by exporting them in a shell that also starts the client. Allowlisted `git` reads run with the repository's filters, diff drivers, hooks, and signing programs disabled; this needs Git 2.31 or newer (older Git makes those shell calls exit 126), and the user's own global Git config still applies.

[Back to README](../../README.md)
