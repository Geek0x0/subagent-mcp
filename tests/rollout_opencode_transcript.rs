//! Projection of Codex-shaped rollout lines to an OpenCode import body.

use serde_json::{Value, json};
use subagent_mcp::rollout::opencode::transcript::{Transcript, new_message_id, new_session_id};

const T0: u64 = 1_794_800_000_000;

fn session_meta() -> (&'static str, Value) {
    (
        "session_meta",
        json!({"id": "t1", "cwd": "/work/repo", "model_provider": "magpie", "originator": "subagent-mcp"}),
    )
}

fn turn_context() -> (&'static str, Value) {
    (
        "turn_context",
        json!({"turn_id": "u1", "cwd": "/work/repo", "model": "deepseek-v4", "effort": "high"}),
    )
}

fn user(text: &str) -> (&'static str, Value) {
    (
        "response_item",
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}),
    )
}

fn assistant(text: &str) -> (&'static str, Value) {
    (
        "response_item",
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}),
    )
}

fn reasoning(text: &str) -> (&'static str, Value) {
    (
        "response_item",
        json!({"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": text}]}),
    )
}

fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> (&'static str, Value) {
    (
        "event_msg",
        json!({"type": "token_count", "info": {
            "total_token_usage": {},
            "last_token_usage": {"input_tokens": input, "cached_input_tokens": cached,
                "output_tokens": output, "reasoning_output_tokens": reasoning, "total_tokens": input + output},
        }}),
    )
}

fn call(id: &str, name: &str, args: &str) -> (&'static str, Value) {
    (
        "response_item",
        json!({"type": "function_call", "call_id": id, "name": name, "arguments": args}),
    )
}

fn output(id: &str, text: &str) -> (&'static str, Value) {
    (
        "response_item",
        json!({"type": "function_call_output", "call_id": id, "output": text}),
    )
}

fn event(kind: &str) -> (&'static str, Value) {
    ("event_msg", json!({"type": kind}))
}

fn feed(lines: &[(&'static str, Value)]) -> Transcript {
    let mut t = Transcript::new();
    for (i, (kind, payload)) in lines.iter().enumerate() {
        t.push_line_at(kind, payload, T0 + (i as u64) * 10);
    }
    t
}

/// Replace every `msg_...` id with `msg_<n>` in order of appearance.
fn normalize(mut body: Value) -> Value {
    let mut n = 0;
    for m in body["messages"].as_array_mut().unwrap() {
        n += 1;
        m["id"] = json!(format!("msg_{n}"));
    }
    body
}

fn messages(body: &Value) -> &Vec<Value> {
    body["messages"].as_array().unwrap()
}

#[test]
fn projects_user_and_text_turn() {
    let t = feed(&[
        session_meta(),
        turn_context(),
        user("fix the bug\nplease"),
        usage(100, 40, 20, 5),
        assistant("done"),
        event("task_complete"),
    ]);
    let body = normalize(t.to_import("ses_test", "prj_1"));
    assert_eq!(body["info"]["id"], "ses_test");
    assert_eq!(body["info"]["projectID"], "prj_1");
    assert_eq!(body["info"]["agent"], "subagent-mcp");
    assert_eq!(
        body["info"]["model"],
        json!({"id": "deepseek-v4", "providerID": "magpie"})
    );
    assert_eq!(body["info"]["title"], "subagent-mcp: fix the bug");
    assert_eq!(body["info"]["location"], json!({"directory": "/work/repo"}));
    assert_eq!(body["location"], json!({"directory": "/work/repo"}));
    assert_eq!(body["info"]["outcome"], "succeeded");
    assert_eq!(
        body["info"]["tokens"],
        json!({"input": 60, "output": 20, "reasoning": 5, "cache": {"read": 40, "write": 0}})
    );
    let m = messages(&body);
    assert_eq!(m.len(), 2);
    assert_eq!(m[0]["type"], "user");
    assert_eq!(m[0]["text"], "fix the bug\nplease");
    assert_eq!(m[1]["type"], "assistant");
    assert_eq!(m[1]["content"], json!([{"type": "text", "text": "done"}]));
    assert_eq!(m[1]["finish"], "stop");
    assert_eq!(m[1]["agent"], "subagent-mcp");
    assert_eq!(m[1]["tokens"]["cache"]["read"], 40);
}

#[test]
fn projects_reasoning_then_text() {
    let t = feed(&[
        session_meta(),
        turn_context(),
        user("q"),
        reasoning("think"),
        assistant("a"),
        event("task_complete"),
    ]);
    let body = t.to_import("ses_test", "prj_1");
    let content = &messages(&body)[1]["content"];
    assert_eq!(content[0]["type"], "reasoning");
    assert_eq!(content[0]["text"], "think");
    assert_eq!(content[1], json!({"type": "text", "text": "a"}));
}

#[test]
fn projects_tool_call_and_output_as_completed_tool_part() {
    let t = feed(&[
        session_meta(),
        turn_context(),
        user("list"),
        call("c1", "shell", r#"{"command":"ls"}"#),
        output("c1", "file.txt"),
        assistant("ok"),
        event("task_complete"),
    ]);
    let body = t.to_import("ses_test", "prj_1");
    let m = messages(&body);
    // The tool result starts no new message by itself; the answer after it is a new model turn.
    assert_eq!(m.len(), 3);
    let tool = &m[1]["content"][0];
    assert_eq!(tool["type"], "tool");
    assert_eq!(tool["id"], "c1");
    assert_eq!(tool["name"], "shell");
    assert_eq!(tool["executed"], true);
    assert_eq!(
        tool["state"],
        json!({"status": "completed", "input": {"command": "ls"},
               "content": [{"type": "text", "text": "file.txt"}]})
    );
    assert_eq!(m[1]["finish"], "tool-calls");
    assert_eq!(m[2]["content"], json!([{"type": "text", "text": "ok"}]));
    assert_eq!(m[2]["finish"], "stop");
}

#[test]
fn marks_tool_error() {
    let mut t = feed(&[
        session_meta(),
        user("x"),
        call("c1", "shell", r#"{"command":"false"}"#),
        output("c1", "boom"),
    ]);
    t.mark_tool_error("c1");
    let body = t.to_import("ses_test", "prj_1");
    assert_eq!(
        messages(&body)[1]["content"][0]["state"],
        json!({"status": "error", "input": {"command": "false"},
               "error": {"type": "tool", "message": "boom"},
               "content": [{"type": "text", "text": "boom"}]})
    );
}

#[test]
fn a_call_without_output_is_running() {
    let t = feed(&[
        session_meta(),
        user("x"),
        call("c1", "shell", r#"{"command":"sleep"}"#),
    ]);
    let body = t.to_import("ses_test", "prj_1");
    assert_eq!(
        messages(&body)[1]["content"][0]["state"],
        json!({"status": "running", "input": {"command": "sleep"}, "metadata": {}})
    );
}

#[test]
fn apply_patch_custom_tool_call_is_a_tool_part() {
    let t = feed(&[
        session_meta(),
        user("patch"),
        (
            "response_item",
            json!({"type": "custom_tool_call", "status": "completed", "call_id": "p1",
                   "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch"}),
        ),
        (
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "p1", "output": "applied"}),
        ),
    ]);
    let body = t.to_import("ses_test", "prj_1");
    let tool = &messages(&body)[1]["content"][0];
    assert_eq!(tool["name"], "apply_patch");
    assert_eq!(
        tool["state"]["input"],
        json!({"patch": "*** Begin Patch\n*** End Patch"})
    );
    assert_eq!(tool["state"]["status"], "completed");
}

#[test]
fn second_model_turn_starts_a_new_assistant_message() {
    let t = feed(&[
        session_meta(),
        user("go"),
        call("c1", "shell", r#"{"command":"ls"}"#),
        output("c1", "x"),
        reasoning("again"),
        assistant("final"),
        event("task_complete"),
    ]);
    let body = t.to_import("ses_test", "prj_1");
    let m = messages(&body);
    assert_eq!(m.len(), 3, "user, tool turn, final turn");
    assert_eq!(m[2]["content"][0]["type"], "reasoning");
    assert_eq!(m[2]["content"][1]["text"], "final");
}

#[test]
fn ignores_duplicate_event_msgs() {
    let plain = feed(&[
        session_meta(),
        user("q"),
        assistant("a"),
        event("task_complete"),
    ]);
    let noisy = feed(&[
        session_meta(),
        (
            "event_msg",
            json!({"type": "task_started", "turn_id": "u1"}),
        ),
        user("q"),
        ("event_msg", json!({"type": "user_message", "message": "q"})),
        assistant("a"),
        (
            "event_msg",
            json!({"type": "agent_message", "message": "a"}),
        ),
        (
            "event_msg",
            json!({"type": "provider_request", "provider": "magpie", "model": "m"}),
        ),
        event("task_complete"),
    ]);
    let strip = |b: Value| {
        let mut b = normalize(b);
        for m in b["messages"].as_array_mut().unwrap() {
            m.as_object_mut().unwrap().remove("time");
        }
        b["info"].as_object_mut().unwrap().remove("time");
        b
    };
    assert_eq!(
        strip(plain.to_import("s", "p")),
        strip(noisy.to_import("s", "p"))
    );
}

#[test]
fn never_emits_provider_replay_payloads() {
    let t = feed(&[
        session_meta(),
        user("q"),
        (
            "response_item",
            json!({"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "visible"}],
                   "encrypted_content": "SECRET-BLOB"}),
        ),
        assistant("a"),
    ]);
    let text = t.to_import("s", "p").to_string();
    assert!(!text.contains("SECRET-BLOB"), "{text}");
    assert!(!text.contains("encrypted"), "{text}");
}

#[test]
fn an_error_event_fails_the_session_and_the_message() {
    let t = feed(&[
        session_meta(),
        user("q"),
        assistant("partial"),
        (
            "event_msg",
            json!({"type": "error", "message": "turn limit"}),
        ),
    ]);
    let body = t.to_import("s", "p");
    assert_eq!(body["info"]["outcome"], "failed");
    assert_eq!(messages(&body)[1]["finish"], "error");
}

#[test]
fn ids_match_the_required_shape() {
    fn check(id: &str, prefix: &str, ms: u64) {
        let rest = id.strip_prefix(prefix).expect("prefix");
        assert_eq!(rest.len(), 12 + 14, "{id}");
        assert!(rest.chars().all(|c| c.is_ascii_alphanumeric()), "{id}");
        assert_eq!(&rest[..12], format!("{:012x}", ms), "{id}");
    }
    check(&new_session_id(T0), "ses_", T0);
    check(&new_message_id(T0), "msg_", T0);
}

#[test]
fn ids_sort_by_time_then_creation_order() {
    let a = new_message_id(T0);
    let b = new_message_id(T0);
    let c = new_message_id(T0 + 1);
    assert!(a < b, "{a} < {b}");
    assert!(b < c, "{b} < {c}");
}

#[test]
fn an_empty_transcript_reports_empty() {
    assert!(Transcript::new().is_empty());
    assert!(!feed(&[session_meta(), user("q")]).is_empty());
}
