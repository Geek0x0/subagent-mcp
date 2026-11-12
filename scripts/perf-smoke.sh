#!/bin/sh
# Performance smoke: Rust subagent-mcp vs the pinned Go 0.13.1 baseline.
#
# Measures, side by side, with a loopback-only mock chat-completions provider:
#   - stripped binary size (bytes and MiB);
#   - startup to `--version` (median of 11 runs);
#   - idle RSS after startup (a serve process that answered initialize);
#   - streaming first-token latency (median of 9 `subagent` tool calls);
#   - 32-concurrent-call throughput (32 `subagent` calls in one server process).
#
# Results are recorded, not release gates: the script fails only on
# measurement/setup errors, never on a regression threshold.
#
# Usage: scripts/perf-smoke.sh [RUST_BIN] [GO_BIN]
#   RUST_BIN  default target/release/subagent-mcp (built if missing)
#   GO_BIN    default target/reference/subagent-mcp (fetched if missing)
#
# Writes docs/release/perf-results.md. Deterministic enough for CI: local mock
# provider, no external network, bounded timeouts everywhere.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
rust_bin="${1:-$root/target/release/subagent-mcp}"
go_bin="${2:-$root/target/reference/subagent-mcp}"
out="$root/docs/release/perf-results.md"

case "${1:-}" in
-h | --help)
	sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
	exit 0
	;;
esac

command -v python3 >/dev/null 2>&1 || {
	echo "perf-smoke: python3 is required" >&2
	exit 1
}

# The Rust release binary: use the release profile; build only when missing.
if [ ! -x "$rust_bin" ]; then
	if [ "$rust_bin" = "$root/target/release/subagent-mcp" ]; then
		echo "perf-smoke: building Rust release binary"
		(cd "$root" && cargo build --release)
	else
		echo "perf-smoke: Rust binary not found or not executable: $rust_bin" >&2
		exit 1
	fi
fi
[ -x "$rust_bin" ] || {
	echo "perf-smoke: Rust binary not found or not executable: $rust_bin" >&2
	exit 1
}

# The pinned Go baseline: never rebuilt here; fetched with its SHA-256 check.
if [ ! -x "$go_bin" ]; then
	if [ "$go_bin" = "$root/target/reference/subagent-mcp" ]; then
		echo "perf-smoke: fetching pinned Go baseline"
		(cd "$root" && scripts/fetch-baseline.sh)
	else
		echo "perf-smoke: Go baseline not found or not executable: $go_bin" >&2
		exit 1
	fi
fi
[ -x "$go_bin" ] || {
	echo "perf-smoke: Go baseline not found or not executable: $go_bin" >&2
	exit 1
}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/perf-smoke.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

cat >"$tmp/driver.py" <<'DRIVER'
"""Rust vs Go performance smoke driver.

Speaks newline-delimited MCP JSON-RPC over stdio to each binary, with a
loopback-only mock chat-completions SSE provider. Prints progress to stdout;
measurement failures raise, which fails the run (results are not gates, but
broken measurements are).
"""

import json
import os
import queue
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

RUST_BIN, GO_BIN, OUT = sys.argv[1:4]

# Mock provider shape: small, fixed, no external network.
FIRST_TOKEN_DELAY = 0.010  # seconds before the first SSE byte
CHUNKS = 24
CHUNK_DELAY = 0.002  # seconds between chunks
CHUNK_TEXT = "ok " * 5  # 15 chars per chunk

VERSION_RUNS = 11
FIRST_TOKEN_RUNS = 9
CONCURRENCY = 32
STEP_TIMEOUT = 60.0  # per tool call, seconds
KEY_VAR = "SUBAGENT_MCP_PERF_MOCK_KEY"
PROMPT = "Reply with exactly: ok"

CONFIG = """[providers.mock]
api = "chat-completions"
base_url = "http://127.0.0.1:{port}"
env_key = "{key_var}"
default_model = "mock-model"
models = [{{ id = "mock-model" }}]
"""


# ---------------------------------------------------------------------------
# Mock provider (chat-completions SSE)
# ---------------------------------------------------------------------------


def _sse_chunk(payload: bytes) -> bytes:
    return f"{len(payload):x}\r\n".encode() + payload + b"\r\n"


class MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # keep CI output quiet
        pass

    def _send_json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/models":
            self._send_json(
                200, {"object": "list", "data": [{"id": "mock-model", "object": "model"}]}
            )
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/chat/completions":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            time.sleep(FIRST_TOKEN_DELAY)
            for _ in range(CHUNKS):
                payload = {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion.chunk",
                    "created": 1,
                    "model": "mock-model",
                    "choices": [
                        {"index": 0, "delta": {"content": CHUNK_TEXT}, "finish_reason": None}
                    ],
                }
                self.wfile.write(_sse_chunk(b"data: " + json.dumps(payload).encode() + b"\n\n"))
                self.wfile.flush()
                time.sleep(CHUNK_DELAY)
            final = {
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            }
            self.wfile.write(_sse_chunk(b"data: " + json.dumps(final).encode() + b"\n\n"))
            self.wfile.write(_sse_chunk(b"data: [DONE]\n\n"))
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def start_mock():
    server = ThreadingHTTPServer(("127.0.0.1", 0), MockHandler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


# ---------------------------------------------------------------------------
# MCP stdio client
# ---------------------------------------------------------------------------


class Mcp:
    def __init__(self, binpath, env, cwd):
        self.p = subprocess.Popen(
            [binpath],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=cwd,
        )
        self.q = queue.Queue()
        self.next_id = 1
        threading.Thread(target=self._read_stdout, daemon=True).start()
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stdout(self):
        assert self.p.stdout is not None
        for line in self.p.stdout:
            self.q.put(line.decode("utf-8", "replace").strip())
        self.q.put(None)

    def _read_stderr(self):
        assert self.p.stderr is not None
        for line in self.p.stderr:
            pass  # drain; startup logs and drop warnings are noise here

    def send(self, obj):
        assert self.p.stdin is not None
        self.p.stdin.write((json.dumps(obj) + "\n").encode())
        self.p.stdin.flush()

    def request(self, method, params):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return rid

    def frame(self, timeout):
        item = self.q.get(timeout=timeout)
        if item is None or not item:
            return None
        return json.loads(item)

    def wait_response(self, rid, timeout=STEP_TIMEOUT):
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for response id {rid}")
            frame = self.frame(remaining)
            if frame is None:
                raise RuntimeError(f"server closed stdout while waiting for id {rid}")
            if frame.get("id") == rid and ("result" in frame or "error" in frame):
                return frame

    def check_result(self, frame, rid):
        if "error" in frame:
            raise RuntimeError(f"request {rid} failed: {frame['error']}")
        result = frame.get("result", {})
        if result.get("isError"):
            raise RuntimeError(f"request {rid} returned a tool error: {result}")

    def close(self):
        try:
            if self.p.stdin:
                self.p.stdin.close()
        except OSError:
            pass
        try:
            self.p.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()


def initialize(m):
    rid = m.request(
        "initialize",
        {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "perf-smoke", "version": "1"},
        },
    )
    m.wait_response(rid)
    m.send({"jsonrpc": "2.0", "method": "notifications/initialized"})


# ---------------------------------------------------------------------------
# Measurements
# ---------------------------------------------------------------------------


def measure_size(binpath, tmp):
    """Stripped size of a copy (never mutates the binary under test)."""
    copy = os.path.join(tmp, "stripped-" + os.path.basename(binpath))
    shutil.copy2(binpath, copy)
    used_strip = False
    if shutil.which("strip"):
        subprocess.run(["strip", "--strip-all", copy], check=True, timeout=120)
        used_strip = True
    size = os.path.getsize(copy)
    os.remove(copy)
    return {"bytes": size, "mib": size / (1024 * 1024), "stripped": used_strip}


def measure_version(binpath):
    subprocess.run(
        [binpath, "--version"], capture_output=True, timeout=30, check=True
    )  # warmup: page cache
    samples = []
    for _ in range(VERSION_RUNS):
        t0 = time.perf_counter()
        subprocess.run([binpath, "--version"], capture_output=True, timeout=30, check=True)
        samples.append((time.perf_counter() - t0) * 1000)
    return {"median_ms": statistics.median(samples), "runs": VERSION_RUNS}


def read_rss_kib(pid):
    with open(f"/proc/{pid}/status", encoding="ascii") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])  # kB == KiB
    raise RuntimeError(f"VmRSS not found for pid {pid}")


def measure_idle_rss(binpath, env, cwd):
    m = Mcp(binpath, env, cwd)
    try:
        initialize(m)
        rid = m.request("tools/list", {})
        m.wait_response(rid)
        time.sleep(1.0)  # settle: runtime pools, GC, buffers
        return {"rss_kib": read_rss_kib(m.p.pid)}
    finally:
        m.close()


def measure_first_token(binpath, env, cwd):
    latencies, totals = [], []
    m = Mcp(binpath, env, cwd)
    try:
        initialize(m)
        for _ in range(FIRST_TOKEN_RUNS):
            t0 = time.perf_counter()
            rid = m.request(
                "tools/call",
                {"name": "subagent", "arguments": {"prompt": PROMPT, "cwd": cwd}},
            )
            first = None
            while True:
                frame = m.frame(STEP_TIMEOUT)
                if frame is None:
                    raise RuntimeError("server closed stdout during tool call")
                if frame.get("method") == "subagent/event":
                    msg = frame.get("params", {}).get("msg", {})
                    if msg.get("type") == "agent_message_delta" and first is None:
                        first = time.perf_counter()
                if frame.get("id") == rid and ("result" in frame or "error" in frame):
                    t1 = time.perf_counter()
                    m.check_result(frame, rid)
                    break
            if first is None:
                raise RuntimeError("no agent_message_delta event seen")
            latencies.append((first - t0) * 1000)
            totals.append((t1 - t0) * 1000)
    finally:
        m.close()
    return {
        "median_ms": statistics.median(latencies),
        "median_total_ms": statistics.median(totals),
        "runs": FIRST_TOKEN_RUNS,
    }


def measure_throughput(binpath, env, cwd):
    m = Mcp(binpath, env, cwd)
    try:
        initialize(m)
        ids = []
        t0 = time.perf_counter()
        for _ in range(CONCURRENCY):
            rid = m.request(
                "tools/call",
                {"name": "subagent", "arguments": {"prompt": PROMPT, "cwd": cwd}},
            )
            ids.append(rid)
        pending = set(ids)
        deadline = time.monotonic() + 120
        while pending:
            if time.monotonic() > deadline:
                raise TimeoutError("32 concurrent calls did not finish in 120 s")
            frame = m.frame(5.0)
            if frame is None:
                raise RuntimeError("server closed stdout during throughput run")
            rid = frame.get("id")
            if rid in pending:
                m.check_result(frame, rid)
                pending.discard(rid)
        wall = time.perf_counter() - t0
    finally:
        m.close()
    return {"wall_s": wall, "calls_per_s": CONCURRENCY / wall, "calls": CONCURRENCY}


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------


def version_string(binpath):
    r = subprocess.run(
        [binpath, "--version"], capture_output=True, text=True, timeout=30, check=True
    )
    return r.stdout.strip()


def pct(rust, go):
    return (rust - go) / go * 100 if go else 0.0


def fmt_signed(value, unit="", digits=2):
    return f"{value:+.{digits}f}{unit}"


def write_report(results, out, mock_port):
    rust, go = results["rust"], results["go"]
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")

    size_r, size_g = rust["size"], go["size"]
    ver_r, ver_g = rust["version"], go["version"]
    start_r, start_g = rust["version_bench"], go["version_bench"]
    rss_r, rss_g = rust["rss"], go["rss"]
    ft_r, ft_g = rust["first_token"], go["first_token"]
    tp_r, tp_g = rust["throughput"], go["throughput"]

    strip_note = "strip --strip-all" if size_r["stripped"] else "strip unavailable; raw copy"

    def row(name, r, g, d, p):
        return f"| {name} | {r} | {g} | {d} | {p:+.1f}% |"

    table = "\n".join(
        [
            "| Metric | Rust | Go 0.13.1 | Δ (Rust − Go) | Δ % |",
            "| --- | --- | --- | --- | --- |",
            row(
                "Stripped binary size (MiB)",
                f"{size_r['mib']:.2f}",
                f"{size_g['mib']:.2f}",
                fmt_signed(size_r["mib"] - size_g["mib"], " MiB"),
                pct(size_r["bytes"], size_g["bytes"]),
            ),
            row(
                f"Startup to `--version`, median of {start_r['runs']} runs (ms)",
                f"{start_r['median_ms']:.2f}",
                f"{start_g['median_ms']:.2f}",
                fmt_signed(start_r["median_ms"] - start_g["median_ms"], " ms"),
                pct(start_r["median_ms"], start_g["median_ms"]),
            ),
            row(
                "Idle RSS after startup (KiB)",
                f"{rss_r['rss_kib']}",
                f"{rss_g['rss_kib']}",
                fmt_signed(rss_r["rss_kib"] - rss_g["rss_kib"], " KiB", 0),
                pct(rss_r["rss_kib"], rss_g["rss_kib"]),
            ),
            row(
                f"Streaming first-token latency, median of {ft_r['runs']} runs (ms)",
                f"{ft_r['median_ms']:.2f}",
                f"{ft_g['median_ms']:.2f}",
                fmt_signed(ft_r["median_ms"] - ft_g["median_ms"], " ms"),
                pct(ft_r["median_ms"], ft_g["median_ms"]),
            ),
            row(
                f"{tp_r['calls']} concurrent calls — wall time (s)",
                f"{tp_r['wall_s']:.2f}",
                f"{tp_g['wall_s']:.2f}",
                fmt_signed(tp_r["wall_s"] - tp_g["wall_s"], " s"),
                pct(tp_r["wall_s"], tp_g["wall_s"]),
            ),
            row(
                f"{tp_r['calls']} concurrent calls — throughput (calls/s)",
                f"{tp_r['calls_per_s']:.2f}",
                f"{tp_g['calls_per_s']:.2f}",
                fmt_signed(tp_r["calls_per_s"] - tp_g["calls_per_s"], " calls/s"),
                pct(tp_r["calls_per_s"], tp_g["calls_per_s"]),
            ),
        ]
    )

    try:
        ncpu = os.cpu_count() or "unknown"
    except Exception:
        ncpu = "unknown"
    uname = " ".join(os.uname())

    doc = f"""# Performance smoke: Rust vs Go 0.13.1

Results are recorded, not release gates: there is no pass/fail threshold, and
regressions here do not block a release. Regenerate with `scripts/perf-smoke.sh`.

Generated: {now}

## Environment

- Host: `{uname}` — {ncpu} CPUs
- Rust binary: `{RUST_BIN}` — `{ver_r}`
- Go baseline: `{GO_BIN}` — `{ver_g}` (pinned v0.13.1 via `tests/differential/baseline.lock`, fetched with SHA-256 check)
- Mock provider: chat-completions SSE on `http://127.0.0.1:{mock_port}` (loopback only, no external network);
  {FIRST_TOKEN_DELAY * 1000:.0f} ms first-token delay, {CHUNKS} chunks × {CHUNK_DELAY * 1000:.0f} ms, then finish.
- Methodology: binary size is after {strip_note} on a copy; startup is the median of {VERSION_RUNS} `--version` runs;
  idle RSS is `/proc/<pid>/status` VmRSS of a serve process after `initialize` + `tools/list` + 1 s settle;
  first-token latency is the median of {FIRST_TOKEN_RUNS} `subagent` tool calls, measured from request sent to the
  first `subagent/event` `agent_message_delta` notification; throughput is {CONCURRENCY} concurrent `subagent`
  calls in one server process, wall time until all responses arrive.

## Results

{table}

## Notes

- Δ % is (Rust − Go) / Go. Lower is better for every metric except throughput.
- Binary size includes the delta in MiB; all other deltas use the row's unit.
- The mock provider and its delays are part of the measured path for both
  binaries, so first-token and throughput numbers are comparable across them
  but are dominated by the mock's fixed delays, not by provider behavior.
"""
    with open(out, "w", encoding="utf-8") as f:
        f.write(doc)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def measure_all(binpath, port, tmp):
    home = tempfile.mkdtemp(prefix="perf-smoke-home.")
    config = os.path.join(home, "config.toml")
    with open(config, "w", encoding="utf-8") as f:
        f.write(CONFIG.format(port=port, key_var=KEY_VAR))
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": home,
        KEY_VAR: "perf-mock-key",
        "SUBAGENT_MCP_CONFIG": config,
    }
    try:
        return {
            "size": measure_size(binpath, tmp),
            "version": version_string(binpath),
            "version_bench": measure_version(binpath),
            "rss": measure_idle_rss(binpath, env, home),
            "first_token": measure_first_token(binpath, env, home),
            "throughput": measure_throughput(binpath, env, home),
        }
    finally:
        shutil.rmtree(home, ignore_errors=True)


def main():
    with tempfile.TemporaryDirectory(prefix="perf-smoke-work.") as tmp:
        server, port = start_mock()
        try:
            results = {}
            for name, binpath in (("rust", RUST_BIN), ("go", GO_BIN)):
                print(f"perf-smoke: measuring {name}: {binpath}", flush=True)
                results[name] = measure_all(binpath, port, tmp)
        finally:
            server.shutdown()
            server.server_close()
        write_report(results, OUT, port)
        print(f"perf-smoke: report written to {OUT}", flush=True)


if __name__ == "__main__":
    main()
DRIVER

echo "perf-smoke: measuring (results are recorded, not gates)"
python3 "$tmp/driver.py" "$rust_bin" "$go_bin" "$out"
echo "perf-smoke: wrote $out"
