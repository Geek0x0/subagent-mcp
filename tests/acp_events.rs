use serde_json::{Value, json};
use subagent_mcp::policy::Sandbox;
use subagent_mcp::provider::acp::events::{
    Stop, ToolCallCache, map_update, permission_request, pick_option, read_only_auto_reject,
    stop_reason,
};

fn upd(update: Value) -> Value {
    json!({"sessionId": "s1", "update": update})
}

fn map(update: Value) -> Option<(Option<Value>, Option<String>)> {
    let mut cache = ToolCallCache::default();
    map_update("s1", &upd(update), &mut cache).map(|m| (m.event, m.text))
}

#[test]
fn agent_message_chunk_maps_to_delta_and_text() {
    let (event, text) =
        map(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}))
            .unwrap();
    assert_eq!(
        event,
        Some(json!({"type":"agent_message_delta","delta":"hi"}))
    );
    assert_eq!(text.as_deref(), Some("hi"));
}

#[test]
fn agent_thought_chunk_maps_to_reasoning_delta() {
    let (event, text) =
        map(json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hmm"}}))
            .unwrap();
    assert_eq!(
        event,
        Some(json!({"type":"agent_reasoning_delta","delta":"hmm"}))
    );
    assert_eq!(text, None);
}

#[test]
fn non_text_content_is_placeholder() {
    let (_, text) =
        map(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"x"}}))
            .unwrap();
    assert_eq!(text.as_deref(), Some("[image]"));
    let (_, text) = map(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"resource_link","uri":"file:///a"}})).unwrap();
    assert_eq!(text.as_deref(), Some("[resource: file:///a]"));
}

#[test]
fn tool_call_maps_to_exec_begin_with_raw_input() {
    let (event, _) = map(json!({"sessionUpdate":"tool_call","toolCallId":"t1","kind":"execute","title":"ls","rawInput":{"cmd":"ls"}})).unwrap();
    assert_eq!(
        event,
        Some(
            json!({"type":"exec_command_begin","call_id":"t1","tool":"execute","command":"ls","raw_input":{"cmd":"ls"}})
        )
    );
}

#[test]
fn tool_call_update_uses_cached_kind_and_title() {
    let mut cache = ToolCallCache::default();
    map_update(
        "s1",
        &upd(json!({"sessionUpdate":"tool_call","toolCallId":"t1","kind":"edit","title":"edit a"})),
        &mut cache,
    );
    let pending = map_update(
        "s1",
        &upd(json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress"})),
        &mut cache,
    );
    assert!(pending.is_none_or(|m| m.event.is_none()));
    let done = map_update(
        "s1",
        &upd(json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","rawOutput":{"ok":true}})),
        &mut cache,
    )
    .unwrap();
    assert_eq!(
        done.event,
        Some(
            json!({"type":"exec_command_end","call_id":"t1","tool":"edit","command":"edit a","status":"completed","raw_output":{"ok":true}})
        )
    );
}

#[test]
fn tool_call_update_failed_maps_to_end() {
    let (event, _) = map(
        json!({"sessionUpdate":"tool_call_update","toolCallId":"t9","status":"failed","kind":"read"}),
    )
    .unwrap();
    let event = event.unwrap();
    assert_eq!(event["type"], "exec_command_end");
    assert_eq!(event["status"], "failed");
    assert_eq!(event["tool"], "read");
}

#[test]
fn plan_maps_to_plan_update() {
    let entries = json!([{"content":"a","status":"pending"}]);
    let (event, _) = map(json!({"sessionUpdate":"plan","entries":entries})).unwrap();
    assert_eq!(event, Some(json!({"type":"plan_update","entries":entries})));
}

#[test]
fn foreign_session_ignored() {
    let mut cache = ToolCallCache::default();
    let params = json!({"sessionId":"other","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}});
    assert!(map_update("s1", &params, &mut cache).is_none());
}

#[test]
fn missing_session_update_ignored() {
    assert!(map(json!({"content":{"type":"text","text":"x"}})).is_none());
    let mut cache = ToolCallCache::default();
    assert!(map_update("s1", &json!("garbage"), &mut cache).is_none());
}

#[test]
fn unknown_kind_ignored() {
    assert!(map(json!({"sessionUpdate":"available_commands_update"})).is_none());
}

fn perm(options: Value) -> Value {
    json!({"sessionId":"s1","toolCall":{"toolCallId":"t1","kind":"execute","title":"rm -rf x",
        "locations":[{"path":"/a"},{"path":"/b"}]},"options":options})
}

#[test]
fn permission_request_fields() {
    let req = permission_request("th", &perm(json!([])));
    assert_eq!(req.thread_id, "th");
    assert_eq!(req.tool, "execute");
    assert_eq!(req.command.as_deref(), Some("rm -rf x"));
    assert_eq!(req.paths, vec!["/a", "/b"]);
    assert_eq!(req.reason, "rm -rf x");
    let req = permission_request("th", &json!({"toolCall":{}}));
    assert_eq!(req.tool, "acp");
}

#[test]
fn pick_prefers_once_over_always() {
    let p = perm(json!([
        {"optionId":"aa","kind":"allow_always"},{"optionId":"ao","kind":"allow_once"},
        {"optionId":"ra","kind":"reject_always"},{"optionId":"ro","kind":"reject_once"}]));
    assert_eq!(
        pick_option(&p, true),
        json!({"outcome":{"outcome":"selected","optionId":"ao"}})
    );
    assert_eq!(
        pick_option(&p, false),
        json!({"outcome":{"outcome":"selected","optionId":"ro"}})
    );
    let p = perm(
        json!([{"optionId":"aa","kind":"allow_always"},{"optionId":"ra","kind":"reject_always"}]),
    );
    assert_eq!(pick_option(&p, true)["outcome"]["optionId"], "aa");
    assert_eq!(pick_option(&p, false)["outcome"]["optionId"], "ra");
}

#[test]
fn pick_no_match_cancels() {
    let p = perm(json!([{"optionId":"aa","kind":"allow_always"}]));
    assert_eq!(
        pick_option(&p, false),
        json!({"outcome":{"outcome":"cancelled"}})
    );
    assert_eq!(
        pick_option(&json!({}), true),
        json!({"outcome":{"outcome":"cancelled"}})
    );
}

#[test]
fn read_only_rejects_execute_not_read() {
    for kind in ["edit", "delete", "move", "execute"] {
        let p = json!({"toolCall":{"kind":kind}});
        assert!(read_only_auto_reject(Sandbox::ReadOnly, &p), "{kind}");
    }
    assert!(!read_only_auto_reject(
        Sandbox::ReadOnly,
        &json!({"toolCall":{"kind":"read"}})
    ));
}

#[test]
fn workspace_write_never_auto_rejects() {
    for kind in ["edit", "delete", "move", "execute"] {
        let p = json!({"toolCall":{"kind":kind}});
        assert!(!read_only_auto_reject(Sandbox::WorkspaceWrite, &p));
        assert!(!read_only_auto_reject(Sandbox::DangerFullAccess, &p));
    }
}

#[test]
fn stop_reasons() {
    assert!(matches!(
        stop_reason(&json!({"stopReason":"end_turn"})),
        Stop::Ok
    ));
    assert!(matches!(
        stop_reason(&json!({"stopReason":"weird"})),
        Stop::Ok
    ));
    assert!(matches!(stop_reason(&json!({})), Stop::Ok));
    assert!(matches!(
        stop_reason(&json!({"stopReason":"cancelled"})),
        Stop::Cancelled
    ));
    for r in ["max_tokens", "max_turn_requests", "refusal"] {
        match stop_reason(&json!({ "stopReason": r })) {
            Stop::Partial(s) => assert_eq!(s, r),
            _ => panic!("{r}"),
        }
    }
}
