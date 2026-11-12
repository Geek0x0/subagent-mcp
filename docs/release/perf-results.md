# Performance smoke: Rust vs Go 0.13.1

Results are recorded, not release gates: there is no pass/fail threshold, and
regressions here do not block a release. Regenerate with `scripts/perf-smoke.sh`.

Generated: 2026-11-12 22:52:38 UTC

## Environment

- Host: `Linux VAN-939705-PC2 7.0.0-43-generic #43~24.04.1-Ubuntu SMP PREEMPT_DYNAMIC Sat Oct 31 06:52:47 UTC 2 x86_64` — 28 CPUs
- Rust binary: `/tmp/opencode/sdd-task29B/target/release/subagent-mcp` — `subagent-mcp 0.14.0`
- Go baseline: `/tmp/opencode/sdd-task29B/target/reference/subagent-mcp` — `subagent-mcp 0.13.1` (pinned v0.13.1 via `tests/differential/baseline.lock`, fetched with SHA-256 check)
- Mock provider: chat-completions SSE on `http://127.0.0.1:35049` (loopback only, no external network);
  10 ms first-token delay, 24 chunks × 2 ms, then finish.
- Methodology: binary size is after strip --strip-all on a copy; startup is the median of 11 `--version` runs;
  idle RSS is `/proc/<pid>/status` VmRSS of a serve process after `initialize` + `tools/list` + 1 s settle;
  first-token latency is the median of 9 `subagent` tool calls, measured from request sent to the
  first `subagent/event` `agent_message_delta` notification; throughput is 32 concurrent `subagent`
  calls in one server process, wall time until all responses arrive.

## Results

| Metric | Rust | Go 0.13.1 | Δ (Rust − Go) | Δ % |
| --- | --- | --- | --- | --- |
| Stripped binary size (MiB) | 5.32 | 20.91 | -15.60 MiB | -74.6% |
| Startup to `--version`, median of 11 runs (ms) | 0.97 | 2.82 | -1.86 ms | -65.8% |
| Idle RSS after startup (KiB) | 6888 | 14644 | -7756 KiB | -53.0% |
| Streaming first-token latency, median of 9 runs (ms) | 52.43 | 51.63 | +0.79 ms | +1.5% |
| 32 concurrent calls — wall time (s) | 0.13 | 0.14 | -0.01 s | -5.8% |
| 32 concurrent calls — throughput (calls/s) | 248.62 | 234.19 | +14.43 calls/s | +6.2% |

## Notes

- Δ % is (Rust − Go) / Go. Lower is better for every metric except throughput.
- Binary size includes the delta in MiB; all other deltas use the row's unit.
- The mock provider and its delays are part of the measured path for both
  binaries, so first-token and throughput numbers are comparable across them
  but are dominated by the mock's fixed delays, not by provider behavior.
