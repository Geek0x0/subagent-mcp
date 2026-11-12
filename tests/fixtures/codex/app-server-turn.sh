#!/bin/sh
# Fake codex app-server fixture that records every client request and drives
# one scripted turn. Used by the differential Codex scenario: the pinned Go
# binary and the Rust adapter both run against this peer, so their
# client-to-peer wire behavior compares equal.
#
# Every incoming line is appended to $FAKE_CODEX_RECORD (one JSON-RPC request
# or notification per line, in arrival order). Replies and event emissions use
# fixed ids so golden comparison only needs UUID/pid-style normalization of
# client-sent volatile fields.
#
# Scripted turn: one agentMessage ("hello from codex") delivered as two
# item/agentMessage/delta events plus the completed item, then turn/completed.
IFS= read -r line || exit 0
# The pool dialer scrubs the child environment, so the record path is a fixed
# well-known file under /tmp rather than an env var. The differential tests
# delete it before and after each run and serialize codex scenarios on a mutex.
RECORD="${FAKE_CODEX_RECORD:-/tmp/subagent-mcp-codex-app-server-record.jsonl}"

while true; do
  printf '%s\n' "$line" >> "$RECORD"
  case "$line" in
    *'"id"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
      method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
      case "$method" in
        initialize)
          printf '{"id":%s,"result":{"userAgent":"codex-cli/0.156.1 (fixture)"}}\n' "$id" ;;
        thread/start)
          printf '{"id":%s,"result":{"thread":{"id":"th_diff1"}}}\n' "$id" ;;
        turn/start)
          printf '{"id":%s,"result":{"turn":{"id":"turn_diff1"}}}\n' "$id"
          printf '%s\n' '{"jsonrpc":"2.0","method":"turn/started","params":{"threadId":"th_diff1","turnId":"turn_diff1"}}'
          printf '%s\n' '{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"th_diff1","turnId":"turn_diff1","delta":"hello "}}'
          printf '%s\n' '{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"threadId":"th_diff1","turnId":"turn_diff1","delta":"from codex"}}'
          printf '%s\n' '{"jsonrpc":"2.0","method":"item/completed","params":{"threadId":"th_diff1","turnId":"turn_diff1","item":{"id":"item_1","type":"agentMessage","text":"hello from codex"}}}'
          printf '%s\n' '{"jsonrpc":"2.0","method":"turn/completed","params":{"threadId":"th_diff1","turn":{"id":"turn_diff1","status":"completed"}}}' ;;
        *)
          printf '{"id":%s,"result":{}}\n' "$id" ;;
      esac
      ;;
  esac
  IFS= read -r line || exit 0
done
