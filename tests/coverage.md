# Go test coverage map (pinned baseline v0.13.1)

Every Go `_test.go` file captured at the pinned baseline
(commit `fe563a12ad30046c0d5be27fdddf9f18d39e8e47`, artifact
`subagent-mcp_0.13.1_linux_amd64.tar.gz`, SHA-256
`7ead1f164a1c8e05a4cf5e5d2f22f19746ffa5f315f44f13b0b505c6034f13dc`)
is listed below — a fixed list of 58 files, captured before the Go tree is
deleted on purpose: after cutover a glob over the (empty) Go tree would
silently pass. `tests/differential/cli_config.rs` pins exactly this list and
fails if the table drifts from it.

Rust coverage columns:

- **unit/integration** — the Rust test files (and, where the behavior lives
  inline, `src/` modules with their own `#[cfg(test)]` suites) that port the
  Go assertions;
- **differential** — Task 27 scenarios in `tests/differential/` that compare
  the Rust behavior against goldens captured from the pinned Go binary
  (`tests/fixtures/`), or replay full MCP transcripts against it.

| # | Go `_test.go` file (pinned baseline) | Rust unit/integration coverage | Differential coverage |
|---|--------------------------------------|--------------------------------|-----------------------|
| 1 | `cmd/subagent-mcp/check_test.go` | `tests/cli.rs`, `src/check.rs` | `tests/differential/cli_config.rs` |
| 2 | `cmd/subagent-mcp/main_test.go` | `tests/cli.rs`, `src/main.rs` | `tests/differential/cli_config.rs` |
| 3 | `cmd/subagent-mcp/runtime_test.go` | `tests/cli.rs` | `tests/differential/cli_config.rs` |
| 4 | `internal/agent/agentsmd_test.go` | `src/agent/prompt.rs`, `tests/agent_runner.rs` | — |
| 5 | `internal/agent/integration_test.go` | `tests/agent_tools.rs` (real executor loop) | `tests/differential/agent.rs` (MCP replays) |
| 6 | `internal/agent/main_test.go` | `tests/agent_runner.rs`, `tests/agent_session.rs` | — |
| 7 | `internal/agent/manager_acquire_test.go` | `tests/agent_session.rs` | — |
| 8 | `internal/agent/rollout_test.go` | `tests/rollout_golden.rs`, `src/agent/rollout.rs` | `tests/differential/rollout.rs` |
| 9 | `internal/agent/runner_nudge_test.go` | `tests/agent_runner.rs` | `tests/differential/agent.rs` (nudge events + wire) |
| 10 | `internal/agent/runner_test.go` | `tests/agent_runner.rs` | `tests/differential/agent.rs` (MCP replays) |
| 11 | `internal/agent/session_boundary_test.go` | `tests/agent_session.rs`, `tests/agent_tools.rs` | — |
| 12 | `internal/agent/session_budget_test.go` | `tests/agent_runner.rs` | `tests/differential/agent.rs` (budget refusal by name) |
| 13 | `internal/agent/session_symlink_race_test.go` | `tests/agent_session.rs` | — |
| 14 | `internal/config/config_test.go` | `tests/config.rs`, `src/config.rs` | — |
| 15 | `internal/decision/client_test.go` | `tests/decision.rs`, `src/decision.rs` | `tests/differential/providers.rs` (Jev wire golden) |
| 16 | `internal/patch/apply_test.go` | `tests/patch_apply.rs`, `src/patch/` | `tests/differential/tools.rs` (apply_patch tree) |
| 17 | `internal/patch/compose_test.go` | `tests/patch_apply.rs` | — |
| 18 | `internal/patch/parse_test.go` | `tests/patch_apply.rs` | — |
| 19 | `internal/policy/policy_test.go` | `tests/policy_path.rs`, `src/policy/` | — |
| 20 | `internal/provider/chatcompletions/adapter_test.go` | `tests/wire_chat.rs`, `src/provider/chat_completions.rs` | `tests/differential/providers.rs` (wire goldens) |
| 21 | `internal/provider/chatcompletions/client_test.go` | `tests/wire_chat.rs`, `tests/provider_transport.rs` | `tests/differential/providers.rs` |
| 22 | `internal/provider/chatcompletions/drain_test.go` | `tests/wire_chat.rs`, `tests/provider_transport.rs` | — |
| 23 | `internal/provider/chatcompletions/env_credentials_test.go` | `tests/wire_chat.rs` (env-credential suites) | — |
| 24 | `internal/provider/chatcompletions/retry_test.go` | `tests/wire_chat.rs` (retry suites) | — |
| 25 | `internal/provider/chatcompletions/routing_test.go` | `tests/wire_chat.rs` (routing/base-URL suites) | — |
| 26 | `internal/provider/chatcompletions/shared_transport_test.go` | `tests/provider_transport.rs`, `src/provider/transport.rs` | — |
| 27 | `internal/provider/codexappserver/adapter_test.go` | `tests/codex_fake.rs`, `src/provider/codex/` | `tests/differential/providers.rs` (fake app-server record golden) |
| 28 | `internal/provider/codexappserver/client_test.go` | `tests/codex_client.rs` | — |
| 29 | `internal/provider/codexappserver/events_test.go` | `tests/codex_fake.rs` (event mapping) | — |
| 30 | `internal/provider/codexappserver/live_test.go` | `tests/codex_fake.rs` (spawns `tests/fixtures/codex/app-server-hold.sh`) | — |
| 31 | `internal/provider/codexappserver/pool_test.go` | `tests/codex_client.rs`, `src/provider/codex/pool.rs` | — |
| 32 | `internal/provider/codexappserver/subscription_test.go` | `tests/codex_client.rs` (subscription/backlog suites) | — |
| 33 | `internal/provider/codexappserver/thread_stray_test.go` | `tests/codex_fake.rs` (stray-turn suites) | — |
| 34 | `internal/provider/codexappserver/thread_test.go` | `tests/codex_fake.rs` (thread lifecycle) | `tests/differential/providers.rs` (turn against scripted peer) |
| 35 | `internal/provider/factory_test.go` | `src/provider/factory.rs`, `tests/mcp_tools.rs` (registry) | — |
| 36 | `internal/provider/live_test.go` | `tests/provider_transport.rs` (live-path guards), `examples/tls_probe.rs` | — |
| 37 | `internal/provider/messages/adapter_test.go` | `tests/wire_messages.rs`, `src/provider/messages.rs` | `tests/differential/providers.rs` (wire golden) |
| 38 | `internal/provider/messages/env_credentials_test.go` | `tests/messages_env_credentials.rs` | — |
| 39 | `internal/provider/messages/shared_transport_test.go` | `tests/provider_transport.rs` | — |
| 40 | `internal/provider/messages/usage_test.go` | `tests/wire_messages.rs` (usage suites) | — |
| 41 | `internal/provider/provider_test.go` | `src/provider/types.rs`, `tests/provider_transport.rs` | — |
| 42 | `internal/provider/responses/adapter_test.go` | `tests/wire_responses.rs`, `src/provider/responses.rs` | `tests/differential/providers.rs` (wire golden) |
| 43 | `internal/provider/responses/env_credentials_test.go` | `tests/wire_responses_env.rs` | — |
| 44 | `internal/provider/responses/shared_transport_test.go` | `tests/provider_transport.rs` | — |
| 45 | `internal/provider/transport_debug_test.go` | `src/provider/transport.rs` (debug capture suites) | — |
| 46 | `internal/provider/transport_timeout_test.go` | `tests/provider_transport.rs` (header-timeout suites) | — |
| 47 | `internal/repo/repo_test.go` | `src/repo.rs` (git_info suites), `tests/rollout_golden.rs` | — |
| 48 | `internal/rollout/rollout_test.go` | `tests/rollout_golden.rs`, `src/rollout.rs` | `tests/differential/rollout.rs` (parsed-line goldens) |
| 49 | `internal/sandbox/beneath_linux_test.go` | `tests/sandbox_linux.rs` | — |
| 50 | `internal/sandbox/sandbox_test.go` | `tests/sandbox_linux.rs`, `tests/agent_tools.rs` | — |
| 51 | `internal/server/auto_test.go` | `tests/mcp_auto.rs`, `src/mcp/auto.rs` | `tests/differential/providers.rs` + `agent.rs` (mock Jev endpoint) |
| 52 | `internal/server/main_test.go` | `tests/mcp_server.rs`, `src/mcp/stdio.rs` | `tests/differential/mcp.rs` |
| 53 | `internal/server/server_test.go` | `tests/mcp_server.rs`, `tests/mcp_tools.rs` | `tests/differential/mcp.rs`, `agent.rs` (MCP replays) |
| 54 | `internal/testutil/fakechat_test.go` | `tests/support/responses_fake.rs`, `tests/support/mod.rs` (harness equivalents) | `tests/differential/support/mock_http.rs` |
| 55 | `internal/tools/git_vectors_test.go` | `tests/tools_files.rs` (git vector tables) | — |
| 56 | `internal/tools/main_test.go` | `tests/agent_tools.rs` (built-in tool table) | — |
| 57 | `internal/tools/procfile_test.go` | `tests/tools_files.rs` (procfile suites) | — |
| 58 | `internal/tools/tools_test.go` | `tests/tools_files.rs`, `tests/tools_shell.rs` | `tests/differential/tools.rs` (shell/read/write outputs vs Go goldens) |

## Golden fixtures captured from the pinned binary (stand alone after Go deletion)

- `tests/fixtures/mcp/*.jsonl` — complete Go output streams (responses and
  `subagent/event` notifications in arrival order) for eleven scenarios:
  chat/responses/messages text turns, tool turn, files turn, patch turn,
  nudge, budget refusal, mid-turn cancellation, auto (Jev) selection, a Codex
  app-server turn, and a progress turn captured with `_meta.progressToken`
  (`chat-progress.jsonl`, whose `notifications/progress` frames — token,
  per-call sequence, mapped message — are compared causally per call).
- `tests/fixtures/wire/<scenario>/NN-*.json` — provider HTTP requests
  (method, path, headers, body) recorded by the mock during the same runs,
  plus `expected-tree.txt` for the files turn.
- `tests/fixtures/rollout/<scenario>.jsonl` — rollout JSONL written by the Go
  server for the same runs. All ten are compared live: the MCP replays in
  `tests/differential/agent.rs` read the rollout file the server wrote under
  `CODEX_HOME` — including the server-written `session_meta` line — after
  each scenario, and `tests/differential/rollout.rs` additionally compares
  the runner-level recorder output (with `session_meta` constructed at the
  call site) for three scenarios.
- `tests/fixtures/patch/` — the apply_patch case (patch text extracted from
  the captured wire request), the resulting tree with modes, and file
  contents.
- `tests/fixtures/codex/app-server-turn.sh` + `turn-record.jsonl` — the
  scripted fake app-server and the client-to-peer request record the Go
  client produced.
- `tests/fixtures/sse/` — SSE byte fixtures (pre-existing, extended with
  `chat/empty_stop_usage.sse`, `chat/apply_patch_add.sse`,
  `chat/tool_calls_files.sse` for the new scenarios).

Volatile fields are normalized by `tests/differential/support/normalize.rs`
(UUIDs, RFC 3339 timestamps, Go durations, PIDs, provider request IDs,
versions, temp roots — in string values and in object keys, since e.g. the
rollout `patch_apply_end` payload keys its `changes` map by file path) plus
the scenario-local extras documented in `tests/differential/support/golden.rs`
(rollout `current_date`, `completed_at`, `cli_version`; timing values
`duration_ms`/`ttft_ms`; transport-only headers `x-stainless-*`, `host`,
`content-length`, `user-agent`, `accept-encoding`, `connection`, and an
`accept: */*` stdlib default). Consecutive `agent_message_delta` events are
collapsed within a turn before comparison, progress is compared per call via
the `chat-progress` replay, and global cross-session interleaving is never
compared.

Known divergence documented by the Codex MCP replay: Go 0.13.1 emits no
`subagent/event` notifications for Codex sessions (verified live against the
pinned binary), while the Rust port forwards the runner events. The Codex
golden comparison pins exactly the frames the golden captured (initialize
response and tools/call result); the extra Rust notifications are a parity
divergence, not asserted behavior.
