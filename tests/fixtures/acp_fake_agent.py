#!/usr/bin/env python3
"""Scripted stdio ACP agent for tests/acp_thread.rs.

Behaviour is chosen by ACP_FAKE_MODE:
  echo                  two agent_message_chunk chunks, then end_turn
  banner                like echo, but prints non-JSON lines first
  permission            request_permission (kind from ACP_FAKE_KIND, default
                        execute) ACP_FAKE_PERMS times (default 1), then echoes
                        the chosen optionIds as text
  hang                  never answers session/prompt until session/cancel
  ignore_cancel         never answers session/prompt, even after cancel
  agent_cancel          answers session/prompt with stopReason cancelled
  auth                  session/new returns -32000
  crash                 exits during session/prompt, after writing stderr
  bad_version           initialize returns protocolVersion 99
  set_model_unsupported session/set_model returns -32601
  set_model_bad_params  session/set_model returns -32602
  stop:<reason>         one chunk, then stopReason <reason>

Every received message is appended to ACP_FAKE_LOG (one JSON per line). The
process id is written to ACP_FAKE_PID when set. When ACP_FAKE_GRANDCHILD is
set, a long-lived `sleep 30` is started in the agent's process group and its
pid is written to that path.
"""

import json
import os
import subprocess
import sys

MODE = os.environ.get("ACP_FAKE_MODE", "echo")
LOG = os.environ.get("ACP_FAKE_LOG")
PID = os.environ.get("ACP_FAKE_PID")
SESSION = "sess-1"

state = {"prompt_id": None, "perms": {}, "chosen": []}


def send(msg):
    msg["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def result(id_, value):
    send({"id": id_, "result": value})


def error(id_, code, message):
    send({"id": id_, "error": {"code": code, "message": message}})


def chunk(text):
    send({
        "method": "session/update",
        "params": {
            "sessionId": SESSION,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            },
        },
    })


def finish(reason):
    result(state["prompt_id"], {"stopReason": reason})
    state["prompt_id"] = None


def on_prompt(id_):
    state["prompt_id"] = id_
    if MODE in ("echo", "banner", "set_model_unsupported", "set_model_bad_params"):
        chunk("hello ")
        chunk("world")
        finish("end_turn")
    elif MODE.startswith("stop:"):
        chunk("partial")
        finish(MODE[len("stop:"):])
    elif MODE == "agent_cancel":
        finish("cancelled")
    elif MODE == "crash":
        sys.stderr.write("fake agent: fatal crash\n")
        sys.stderr.flush()
        os._exit(3)
    elif MODE == "permission":
        kind = os.environ.get("ACP_FAKE_KIND", "execute")
        for n in range(int(os.environ.get("ACP_FAKE_PERMS", "1"))):
            pid = "perm-%d" % n
            state["perms"][pid] = None
            send({
                "id": pid,
                "method": "session/request_permission",
                "params": {
                    "sessionId": SESSION,
                    "toolCall": {"toolCallId": "tc-%d" % n, "title": "run ls", "kind": kind},
                    "options": [
                        {"optionId": "opt-allow", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "opt-reject", "name": "Reject", "kind": "reject_once"},
                    ],
                },
            })
    # hang / ignore_cancel: no answer


def on_permission_answer(id_, msg):
    if id_ not in state["perms"]:
        return
    outcome = (msg.get("result") or {}).get("outcome") or {}
    state["perms"][id_] = outcome
    if any(v is None for v in state["perms"].values()) or state["prompt_id"] is None:
        return
    outcomes = list(state["perms"].values())
    state["perms"] = {}
    if any(o.get("outcome") == "cancelled" for o in outcomes):
        finish("cancelled")
        return
    chunk(",".join(o.get("optionId", "") for o in outcomes))
    finish("end_turn")


def handle(msg):
    method = msg.get("method")
    id_ = msg.get("id")
    if method is None:
        on_permission_answer(id_, msg)
        return
    if method == "initialize":
        version = 99 if MODE == "bad_version" else 1
        auth = [{"id": "login", "name": "Log in"}] if MODE == "auth" else []
        result(id_, {"protocolVersion": version, "agentCapabilities": {}, "authMethods": auth})
    elif method == "session/new":
        if MODE == "auth":
            error(id_, -32000, "Authentication required")
        else:
            result(id_, {"sessionId": SESSION})
    elif method == "session/set_model":
        if MODE == "set_model_unsupported":
            error(id_, -32601, "Method not found")
        elif MODE == "set_model_bad_params":
            error(id_, -32602, "Invalid params")
        else:
            result(id_, {})
    elif method == "session/prompt":
        on_prompt(id_)
    elif method == "session/cancel":
        if MODE == "ignore_cancel":
            return
        if state["prompt_id"] is not None and not state["perms"]:
            finish("cancelled")
    elif id_ is not None:
        error(id_, -32601, "Method not found")


def main():
    if PID:
        with open(PID, "w") as f:
            f.write(str(os.getpid()))
    grandchild = os.environ.get("ACP_FAKE_GRANDCHILD")
    if grandchild:
        p = subprocess.Popen(
            ["sleep", "30"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        with open(grandchild, "w") as f:
            f.write(str(p.pid))
    if MODE == "banner":
        print("Welcome to the fake agent!", flush=True)
        print("[log] starting up", flush=True)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        if LOG:
            with open(LOG, "a") as f:
                f.write(line + "\n")
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        handle(msg)


main()
