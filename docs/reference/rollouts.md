# Rollout files

Every session writes a Codex-compatible rollout as JSONL under `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<YYYY-MM-DDTHH-MM-SS>-<threadId>.jsonl`, using UTC and `~/.codex` when `CODEX_HOME` is unset. Directories are created `0700` and the file is `0600`. Each line is `{"timestamp": ..., "type": ..., "payload": {...}}` with the Codex line types `session_meta`, `turn_context`, `response_item`, and `event_msg`. Codex ecosystem tools can read these files for usage reporting, session viewing, and audit; `originator` is `subagent-mcp`, `model_provider` is the selected provider's config name, `turn_context` records both the caller's `effort` and the mapped `effort_sent`, and `reasoning` items come from the provider's human-readable reasoning text and are omitted when empty. `session_meta` records the working directory, the full composed system prompt, and the Git branch and commit when `cwd` is in a repository. Replay payloads are never written. Because the files are written but never read back, these sessions are visible to `codex resume` but cannot actually be resumed by Codex.

Set `SUBAGENT_MCP_ROLLOUT=off` to disable rollout writing. A rollout write or open failure logs one line to stderr and disables the rollout for that session; it never fails an agent run.

The rollout records the prompt, the composed system prompt, tool arguments, tool results, and command output, so it can contain secrets and file contents. Treat the rollout directory with the same care as the session transcript.

### OpenCode format

By default a session is recorded as a Codex rollout (above). Set `[rollout] format = "opencode"` (or `SUBAGENT_MCP_ROLLOUT_FORMAT=opencode`) to record it in [OpenCode](https://opencode.ai)'s session list instead, so the subagent sessions can be browsed in OpenCode. The two formats are mutually exclusive: one process writes one format. `SUBAGENT_MCP_ROLLOUT=off` disables both. It applies to the native providers (chat completions, responses, messages); the Codex provider keeps writing its own rollout.

```toml
[rollout]
format = "opencode"          # default "codex"

[rollout.opencode]
mode = "api"                 # "api" (default) or "db"
fallback = "db"              # "db" (default) or "none"; only used when mode = "api"
# bin = "/path/to/opencode"  # default: `opencode` on PATH
# db_path = "/path/to/opencode.db"  # default: $OPENCODE_DB, then $XDG_DATA_HOME/opencode/opencode.db, then ~/.local/share/opencode/opencode.db
```

How it works:

- After every run, the session's transcript (prompts, answers, reasoning, tool calls and their results) is written **in the background**; a run never waits for OpenCode and never fails because of it. The session appears under the OpenCode project that matches its working directory: the git root, else the nearest ancestor directory OpenCode knows as a project. OpenCode's global project is never used; with no matching project the write is skipped and a warning says so.
- `mode = "api"` uses the official import endpoint through `opencode api`, so OpenCode handles authentication and this server never sees a credential. That endpoint is **experimental** in OpenCode; the format was verified against OpenCode 2.3.x. A different version produces a warning, and the API is still tried. The transcript is passed to `opencode` as **one command-line argument**, so local users can see it in `ps` while the call runs (up to about 100 KB, including prompts and command output), unlike the 0600 Codex rollout; on a shared host use `mode = "db"`. A transcript larger than about 100,000 bytes cannot be passed on a command line and counts as an API failure. Real sessions often exceed that, so with the default `fallback = "db"` many sessions are written through the database, with a warning in the next reply; an API answer of 400/422-style (the server judged the transcript invalid) is never retried through the database; with `fallback = "none"` such a session stays at its last copy under 100 KB and the recorder is disabled after the first failure. The child process runs with the same scrubbed environment as the shell tool (no provider keys, no `SUBAGENT_MCP_*`) and in `/`. Linux and macOS are supported; Windows is unverified (its command-line limit is much lower, so the limit there is 12,000 bytes). On the API path `opencode session delete` also deletes any child sessions of the replaced session.
- `fallback = "db"` writes OpenCode's SQLite database directly when the API path fails (for example when `opencode` is not installed). `mode = "db"` uses only the database and never runs `opencode`; the OpenCode version is then taken from the newest session already stored in the database (not from the installed binary), so a database with no sessions yet fails with "OpenCode version unknown". With `fallback = "none"` and no `opencode` binary, nothing is recorded: you only get the warning.
- **The database is owned by another program** (and also holds its account credentials), so this path is deliberately narrow: Linux and macOS only; only OpenCode 2.3.x is accepted (anything else is refused); the file must already exist; one transaction per write that touches only the session, message and sequence tables; no migration, no table creation, foreign keys off so nothing cascades, and no checkpointing. The write-ahead log (`opencode.db-wal`) therefore keeps growing, by the size of the transcript on every run, until OpenCode's next commit checkpoints it (this can happen while OpenCode is idle too), and a backup of `opencode.db` alone would miss the unchecked writes. Only an OpenCode database whose latest migration is exactly the verified one is written. Prefer the API.
- **Each run replaces the whole session in OpenCode** (the import endpoint cannot append): the previous copy is deleted and the full transcript imported again. Anything you did to that session inside OpenCode, such as continuing it or renaming it, is overwritten by the next run.
- A failure is reported once per cause, on stderr and as a `warning:` line at the top of that session's next reply; a failure of a session's last run has no later reply to carry it, so it reaches stderr only. When every allowed path has failed, writing stops for that session. At shutdown the server waits for pending writes within the shutdown grace period; a write still running when the process ends, or a forced kill, loses the last run, and one that is cut between the delete and the import loses the session.
- The transcript, including command output and file contents, lives in OpenCode's database afterwards and is subject to OpenCode's own features such as sharing (this server never shares a session). Treat it like the Codex rollout.

[Back to README](../../README.md)
