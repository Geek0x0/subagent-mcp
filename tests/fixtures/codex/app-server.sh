#!/bin/sh
# Fake Codex app-server fixture used by tests/codex_fake.rs.
# Pinned client version: codex-cli 0.156.1
#
# Completes the initialize/initialized handshake, then exits when stdin closes.
# The pool's graceful close path must reap this direct child without killing it.
IFS= read -r _line || exit 0
printf '%s\n' '{"id":1,"result":{"userAgent":"codex-cli 0.156.1 (fixture)"}}'
IFS= read -r _line || exit 0
cat >/dev/null
