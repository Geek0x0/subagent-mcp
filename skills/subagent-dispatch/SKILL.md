---
name: subagent-dispatch
description: Dispatch coding work units to the subagent MCP tools (subagent / subagent-reply, or the names set by SUBAGENT_MCP_TOOL_NAME). Use when delegating implementation work to a configured model provider or continuing an existing subagent thread.
---

# Subagent Dispatch

The MCP tool names follow the server's `SUBAGENT_MCP_TOOL_NAME` environment variable, which defaults to `subagent`. The server's config file can define more than one model provider (DeepSeek over Chat Completions, OpenAI over Responses, Anthropic over Messages, or others); the caller selects which one a session uses via the `provider` argument on the start tool.

With an experimental `codex-app-server` provider, the session inherits the user's Codex environment and uses Codex's own tools and sandbox, not the native tool and Landlock rules below. `config.max_turns` has no effect; Codex loads AGENTS.md, and a non-empty `base-instructions` replaces Codex's base prompt only when supplied. Codex owns the rollout under `~/.codex/sessions` (or the configured Codex home); `SUBAGENT_MCP_ROLLOUT=off` does not disable it. Authentication uses `codex login`, not `env_key`.

## New session vs reply

Call `subagent` for a new, independent work unit. Start a new session whenever the task needs a different working directory, model, sandbox, approval policy, instruction set, or turn limit. These settings are fixed when the session is created.

Call `subagent-reply` for another step in the same task: answer a question from the agent, clarify or correct its work, provide test-failure output, or request another verification pass. A reply accepts only `threadId` and `prompt`; the original settings carry over unchanged and the accumulated conversation history is reused.

A `busy` error means another call is already processing that thread. Wait for that call to finish, then retry the reply; use a new session only if the work is genuinely independent. An `unknown threadId` error means the server has no session with that ID, commonly because the ID is wrong or the server restarted. Check the ID, or start a new `subagent` session if the original session is no longer in memory. Idle threads are evicted after 24 hours, and when more than 256 sessions exist the least recently used idle ones are evicted; an evicted `threadId` returns `unknown threadId` just like a server restart. Callers can also cancel a running call with standard MCP cancellation; the call returns an error and the thread stays resumable with `subagent-reply`.

## threadId

For every response associated with a newly created or existing session, read `structuredContent.threadId` and retain it. It is present on successful responses and on execution errors, including `busy`, so a session remains available for follow-up after an error. Argument-validation failures before a session is created and an `unknown threadId` reply cannot provide a resumable session ID.

Use the most recent returned ID with `subagent-reply`; the ID remains the same for the life of the session. Sessions exist only in the server process's memory and do not survive a restart.

## Parameter selection

`model` must be one of the ids in the selected provider's `models` list; the tool schema's `enum` is the union of every configured provider's models. Omit it to use the selected provider's `default_model`. An unknown id is rejected with an error listing the available models; do not reuse a model name from a different provider.

`provider` selects which configured provider a new session uses; the tool schema's `enum` lists every name defined under `[providers]`. Omit it only when the config defines exactly one provider — with two or more, omitting it is a tool error listing the available names, so pass the name the user (or the dispatch policy) selected before the first call of a new session. It is fixed for the session's lifetime; `subagent-reply` always continues with the same provider, and `model` must come from that provider's list.

Choose the narrowest sandbox that permits the task:

- `read-only` (default) permits `read_file` and the policy's shell-command allowlist. Other operations fall outside the sandbox. Auto-allowed shell commands run with no writable roots.
- `workspace-write` permits `read_file`, all `shell` calls, and `write_file` when its resolved path stays inside `cwd`. A `write_file` path containing `..` or escaping through a symlink is outside the sandbox. Auto-allowed shell commands are wrapped in a Landlock ruleset whose writable roots are `cwd`, `/tmp`, `$TMPDIR`, and any `config.writable_roots` entries, bound to their directory inodes when the session starts. Renaming a root or replacing its path cannot redirect the writable boundary; the shell stays in the original cwd inode, while `write_file` and `apply_patch` fail closed if the original cwd path no longer names it. Writes anywhere else fail with `Permission denied` instead of being heuristically detected. Tools that write caches under `$HOME` (for example `go test` writing `~/.cache/go-build`) need that directory listed in `config.writable_roots`.
- `danger-full-access` treats every built-in tool operation as inside the sandbox and runs shell commands without the kernel wrapper.

In both wrapped modes `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`, and `/dev/tty` stay writable; the rest of `/dev`, including `/dev/shm`, does not.

The shell-command allowlist contains `ls`, `cat`, `head`, `tail`, `rg`, `grep`, `find`, `pwd`, `wc`, `stat`, `which`, and `echo`, plus `git status`, `git diff`, `git log`, `git show`, `git branch`, `git blame`, `git rev-parse`, and `git ls-files`.

`read_file` accepts optional `offset` (1-based start line) and `limit` (line count) arguments so large files can be read in slices. The loaded config file is protected: `read_file` refuses it so provider topology stays out of model context.

`apply_patch` is treated as one `write_file` request per touched path, and a denial on any path denies the whole patch; all approval-requiring paths are combined into a single approval request.

Shell output is capped at 16 KiB (the start is kept, the end dropped) and the default shell timeout is 60 seconds, clamped to 600. The kernel sandbox fails closed: on a kernel without Landlock (Linux below 5.13) or on a non-Linux platform, an auto-allowed shell call exits with code 126 and a `subagent-mcp: landlock unavailable: ...` message rather than running unsandboxed. Shell calls a human approves run without the wrapper, and `danger-full-access` never applies it.

**Safety: the policy prevents accidental misuse, and the Landlock wrapper confines auto-allowed shell writes to the roots above. Reads and network access remain unrestricted, `write_file` and `apply_patch` writes are kept beneath `cwd` by a path check and, on Linux, by `openat2(RESOLVE_BENEATH)` rather than by Landlock, and on Landlock ABI v1 (Linux 5.13–5.18) renaming or hard-linking into a different directory always fails with `EXDEV`, and below Landlock ABI v3 (Linux 6.2) truncating existing files outside the writable roots is not restricted. Use stronger operating-system isolation when the trust boundary requires it.**

The `approval-policy` determines what happens to operations inside or outside the selected sandbox:

- `untrusted` allows `read_file` and allowlisted shell commands, and asks for approval for every other operation.
- `on-request` (default) allows operations inside the sandbox and asks for approval outside it.
- `on-failure` currently has exactly the same behavior as `on-request`.
- `never` allows operations inside the sandbox and denies operations outside it without asking.

Approval requests are sent to the MCP client. If approval elicitation is unavailable or unanswered for five minutes, the request is denied.

`cwd` must be an absolute path to an existing directory; a Git worktree root is usually a sensible choice. If omitted, it defaults to the subagent-mcp process working directory.

Use `reasoning-effort` to control reasoning depth. It accepts `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max` and defaults to `high`. The value is passed through the selected provider's `effort_map` before it is sent (for example, the example DeepSeek provider maps `medium` to `high` and `xhigh` to `max`), so choose `low` for simple, fast tasks, `high` for typical work, and `xhigh` or `max` for the hardest multi-step or planning-heavy tasks. The top-level argument wins over `config.model_reasoning_effort`, and an invalid value from either source is an error.

The optional loose `config` object recognizes three keys. `max_turns` accepts a number from 1 through 100000 and overrides the default limit of 50 agent turns; unknown keys, values of the wrong type, and out-of-range values are silently ignored. `model_reasoning_effort` accepts the same values as `reasoning-effort` and is overridden by the top-level argument. `writable_roots` is an array of absolute paths to existing directories that the shell may also write under `workspace-write`. Invalid `model_reasoning_effort` or `writable_roots` values fail the call.

The repository's `AGENTS.md` files are loaded automatically: the server collects them from the repository root down to `cwd` (32 KiB total cap) and appends them to the system prompt between the base instructions and any `developer-instructions`. Include those conventions in the dispatch prompt only when you want to override or extend them, and use `base-instructions` only to replace the built-in base instructions completely.

## Event stream

While a call runs, the server sends `subagent/event` notifications containing the outer fields `threadId` and `msg`. The `msg.type` values are:

- `task_started`: the call was accepted and processing began.
- `agent_message_delta`: streamed assistant text; `delta` contains the new text fragment.
- `token_count`: token usage reported for a model turn, with `prompt_tokens`, `completion_tokens`, and `total_tokens`.
- `exec_command_begin`: tool execution is starting; includes `call_id`, `tool`, and either `command` for `shell`, `paths` (an array) for `apply_patch`, or `path` for the other file tools.
- `exec_command_end`: tool execution finished; includes `call_id` and `tool`, plus `exit_code` for `shell`, `paths` for `apply_patch`, and `error` when execution failed.
- `agent_message`: the final assistant response, in `message`.
- `task_complete`: the thread call completed successfully.
- `error`: the call failed, with the failure text in `message`.

Policy denials and rejected approval requests are returned to the model as tool results; a tool that never begins execution does not emit the begin/end pair.

When a `tools/call` includes `_meta.progressToken`, the server also sends standard `notifications/progress` notifications (a rising `progress` value plus a short `message` such as `started`, `shell: <cmd>`, `completed`, or `error: <msg>`); the `subagent/event` notifications are unchanged.

## Rollout files

Each session also writes a Codex-compatible rollout to `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<threadId>.jsonl` (default `~/.codex`, UTC, mode `0600`). It records the prompt, the composed system prompt, tool arguments, tool results, and command output, so avoid dispatching secrets and treat the directory like a session transcript. Set `SUBAGENT_MCP_ROLLOUT=off` in the server environment to disable it.

## Recommended prompt structure

Give each work unit an explicit goal, a bounded file scope, constraints, verification commands, and the report format you expect. Include enough context for the agent to finish without guessing, while keeping unrelated work out of scope.

Example:

```text
Goal: Add table-driven tests for ParseDuration covering valid units and malformed input.

Scope:
- You may edit internal/config/duration_test.go only.
- Read internal/config/duration.go as needed.

Constraints:
- Do not change production code, dependencies, or generated files.
- Preserve the repository's existing test style.

Verification:
- Run: go test ./internal/config
- Run: go test ./... -race

Report:
- Summarize the cases added.
- List changed files.
- Give each verification command and its result.
- If blocked, report the exact blocker without broadening scope.
```
