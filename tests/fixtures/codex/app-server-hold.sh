#!/bin/sh
# Fake Codex app-server fixture that ignores stdin EOF after the handshake.
# Pinned client version: codex-cli/0.156.1
#
# Used to prove the pool closes stdin, waits its 5 s grace period, then kills
# the direct child. Descendants are deliberately not asserted.
IFS= read -r _line || exit 0
printf '%s\n' '{"id":1,"result":{"userAgent":"codex-cli/0.156.1 (fixture)"}}'
IFS= read -r _line || exit 0
exec sleep 300
