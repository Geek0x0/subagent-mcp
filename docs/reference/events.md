# Event reference

During a running call, the server emits `subagent/event` notifications with `threadId` and a `msg` object. The possible `msg.type` values are:

| Type | Meaning and fields |
|---|---|
| `task_started` | Processing began. |
| `agent_message_delta` | Streamed assistant text in `delta`. Consecutive deltas are merged into one event (at most every 50 ms or 8 KiB, with the tail flushed when the turn ends), so append `delta` to a buffer rather than assuming one event per token — a busy client would otherwise overflow its notification channel and lose most of the stream. |
| `token_count` | Model-turn usage in `prompt_tokens` (the full prompt, cached tokens included), `completion_tokens`, and `total_tokens` (their sum). |
| `exec_command_begin` | Tool execution began; includes `call_id`, `tool`, and `command` for shell, `paths` (array) for `apply_patch`, or `path` for `read_file` and `write_file`. |
| `exec_command_end` | Tool execution ended; includes `call_id`, `tool`, `exit_code` for shell, `paths` for `apply_patch`, and `error` when execution failed. |
| `provider_request` | One model API call finished: `provider`, `model`, `duration_ms`, `ttft_ms` (only when text streamed), `upstream` (when the API names the serving channel) and `error` (only on failure). |
| `agent_nudge` | A reply without a tool call was answered with a continue prompt (see [Models that stop to narrate](configuration.md#models-that-stop-to-narrate)); the prompt is in `message`. |
| `agent_message` | Final assistant text in `message`. |
| `task_complete` | The call completed successfully. |
| `error` | The call failed; `message` contains the error. |

Operations stopped by policy or denied approval do not begin execution and therefore do not emit `exec_command_begin` or `exec_command_end`. For `codex-app-server` sessions Codex reports `item/started` before it requests approval, so a file change that is then declined has already emitted `exec_command_begin` and still emits `exec_command_end`; those events show what Codex reported, not whether the approval was granted.

When a `tools/call` carries `_meta.progressToken`, the server additionally sends standard `notifications/progress` notifications for that call alongside the unchanged `subagent/event` notifications. The `progress` value increases from 1, and `message` carries a short summary: `started`, `shell: <cmd>`, `apply_patch: <paths>`, `<tool>: <path>`, `agent: <first line>`, `completed`, or `error: <msg>`, each at most 200 characters.

[Back to README](../../README.md)
