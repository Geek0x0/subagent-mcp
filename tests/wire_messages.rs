//! Task 12: Anthropic Messages adapter wire tests.
//!
//! Ports `internal/provider/messages/{adapter,usage,env_credentials,shared_transport}_test.go`
//! against a local HTTP mock that records requests and serves scripted SSE, plus
//! the Messages-specific SSE framing cases from the design: blank-line dispatch,
//! empty events dispatched, the 32 MiB line cap, multiple `data:` lines
//! concatenated with `\n`, `[DONE]` treated as ordinary data, EOF without a
//! `message_delta`, and EOF with a non-empty `stop_reason` but no `message_stop`.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use subagent_mcp::provider::messages::{
    MAX_PAUSE_CONTINUATIONS, MAX_RETRIES, Messages, RetryDelay,
};
use subagent_mcp::provider::{
    ModelLister, Provider, ProviderError, Role, ToolCall, ToolSpec, TurnRequest, TurnResult, Usage,
};
use subagent_mcp::util::run::RunContext;
use support::{Mock, ScriptBlock, ScriptMessage, Scripted, Truncate, request, run_turn, user_only};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "run".to_string(),
        parameters: serde_json::value::to_raw_value(&json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        }))
        .unwrap(),
    }
}

async fn one_turn(
    mock: &Mock,
    max_output_tokens: i64,
    req: TurnRequest,
) -> (Result<TurnResult, ProviderError>, String) {
    let provider = Messages::new("anthropic", &mock.config(max_output_tokens)).unwrap();
    run_turn(&provider, req).await
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/sse/messages/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|error| panic!("read fixture {path}: {error}"))
}

async fn run_fixture(name: &str) -> (Result<TurnResult, ProviderError>, String) {
    let mock = Mock::start(vec![Scripted::sse(fixture(name))]).await;
    one_turn(&mock, 1000, request("claude-x", user_only())).await
}

fn assistant_from(result: &TurnResult) -> subagent_mcp::provider::Message {
    subagent_mcp::provider::Message {
        role: Role::Assistant,
        text: result.text.clone(),
        tool_calls: result.tool_calls.clone(),
        opaque: result.opaque.clone(),
        ..Default::default()
    }
}

fn tool_result(call: &ToolCall, text: &str, is_error: bool) -> subagent_mcp::provider::Message {
    subagent_mcp::provider::Message {
        role: Role::Tool,
        text: text.to_string(),
        tool_call_id: Some(call.id.clone()),
        is_error,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Request shape: adaptive summarized thinking, cache control, effort, tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_turn_request_shape() {
    let script = ScriptMessage {
        blocks: vec![
            ScriptBlock::Thinking {
                thinking: "hmm".to_string(),
                signature: "sig-1".to_string(),
            },
            ScriptBlock::Text("hello".to_string()),
        ],
        input_tokens: 10,
        cache_read_tokens: 5,
        cache_creation_tokens: 2,
        output_tokens: 3,
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let mut req = request("claude-x", user_only());
    req.effort = Some("xhigh".to_string());
    req.system = "sys".to_string();
    req.tools = vec![shell_spec()];

    let (result, deltas) = one_turn(&mock, 64000, req).await;
    let result = result.expect("turn");

    assert_eq!(result.text, "hello");
    assert_eq!(deltas, "hello");
    assert_eq!(result.reasoning.as_deref(), Some("hmm"));
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 17,
            cached: 5,
            output: 3,
            reasoning: 0,
            total: 20,
        })
    );

    let recorded = mock.request(0);
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/messages");
    assert_eq!(recorded.header("x-api-key"), Some("k"));
    assert_eq!(recorded.header("anthropic-version"), Some("2023-06-01"));
    assert!(
        recorded
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json")),
        "content type: {:?}",
        recorded.header("content-type")
    );

    let body = recorded.json();
    assert_eq!(body["model"], json!("claude-x"));
    assert_eq!(body["max_tokens"], json!(64000));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(body["output_config"], json!({"effort": "xhigh"}));
    assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(body["system"], json!([{"type": "text", "text": "sys"}]));

    let tool = &body["tools"][0];
    assert_eq!(tool["name"], json!("shell"));
    assert_eq!(tool["description"], json!("run"));
    assert_eq!(tool["eager_input_streaming"], json!(true));
    assert_eq!(
        tool["input_schema"],
        json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        })
    );
}

#[tokio::test]
async fn every_schema_keyword_is_forwarded() {
    let mock = Mock::start(vec![ScriptMessage::text("ok").scripted()]).await;
    let spec = ToolSpec {
        name: "shell".to_string(),
        description: String::new(),
        parameters: serde_json::value::to_raw_value(&json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false,
            "title": "Shell",
            "$defs": {"n": {"type": "number"}}
        }))
        .unwrap(),
    };
    let mut req = request("claude-x", user_only());
    req.tools = vec![spec];

    one_turn(&mock, 1000, req).await.0.expect("turn");

    let schema = mock.request(0).json()["tools"][0]["input_schema"].clone();
    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["title"], json!("Shell"));
    assert_eq!(schema["$defs"], json!({"n": {"type": "number"}}));
    assert_eq!(schema["properties"], json!({"command": {"type": "string"}}));
    assert_eq!(schema["required"], json!(["command"]));
}

#[tokio::test]
async fn malformed_tool_schemas_are_reported() {
    // A schema that is not a JSON object at all, plus the two `required`
    // shapes the Messages API cannot express.
    for (parameters, want) in [
        (json!([]), "tool shell schema"),
        (json!({"required": "command"}), "required must be an array"),
        (json!({"required": [1]}), "required must list only names"),
    ] {
        let mock = Mock::start(Vec::new()).await;
        let spec = ToolSpec {
            name: "shell".to_string(),
            description: String::new(),
            parameters: serde_json::value::to_raw_value(&parameters).unwrap(),
        };
        let mut req = request("claude-x", user_only());
        req.tools = vec![spec];
        let (result, _) = one_turn(&mock, 1000, req).await;
        let error = result.expect_err("malformed schema must fail");
        assert!(
            error.message.contains(want),
            "error {:?} does not contain {want:?}",
            error.message
        );
        assert_eq!(mock.request_count(), 0, "no request may be sent");
    }
}

// ---------------------------------------------------------------------------
// Opaque replay, tool-result grouping, thinking signatures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replays_thinking_and_groups_tool_results() {
    let scripts = [
        ScriptMessage {
            blocks: vec![
                ScriptBlock::Thinking {
                    thinking: "plan".to_string(),
                    signature: "sig-1".to_string(),
                },
                ScriptBlock::Tool {
                    id: "tu_1".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![r#"{"command":"ls"}"#.to_string()],
                },
                ScriptBlock::Tool {
                    id: "tu_2".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![r#"{"command":"pwd"}"#.to_string()],
                },
            ],
            ..ScriptMessage::default()
        },
        ScriptMessage {
            blocks: vec![
                ScriptBlock::Thinking {
                    thinking: "again".to_string(),
                    signature: "sig-2".to_string(),
                },
                ScriptBlock::Tool {
                    id: "tu_3".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![r#"{"command":"cat a"}"#.to_string()],
                },
            ],
            ..ScriptMessage::default()
        },
        ScriptMessage::text("done"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();

    let mut history = vec![subagent_mcp::provider::Message {
        role: Role::User,
        text: "go".to_string(),
        ..Default::default()
    }];
    for turn in 0..3 {
        let mut req = request("claude-x", history.clone());
        req.tools = vec![shell_spec()];
        let (result, _) = run_turn(&provider, req).await;
        let result = result.expect("turn");
        if turn == 0 {
            assert_eq!(result.tool_calls.len(), 2);
            assert_eq!(result.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        }
        history.push(assistant_from(&result));
        for (index, call) in result.tool_calls.iter().enumerate() {
            history.push(tool_result(call, &format!("out-{}", call.id), index == 1));
        }
    }

    let messages = mock.request(2).json()["messages"].clone();
    let messages = messages.as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        5,
        "want user, assistant, grouped results, assistant, grouped results"
    );
    assert_eq!(messages[1]["content"][0]["type"], json!("thinking"));
    assert_eq!(messages[1]["content"][0]["signature"], json!("sig-1"));
    assert_eq!(messages[3]["content"][0]["signature"], json!("sig-2"));

    let results = messages[2]["content"].as_array().expect("tool results");
    assert_eq!(results.len(), 2, "consecutive tool results are one message");
    assert_eq!(results[0]["type"], json!("tool_result"));
    assert_eq!(results[0]["tool_use_id"], json!("tu_1"));
    assert_eq!(results[1]["tool_use_id"], json!("tu_2"));
    assert_eq!(results[1]["is_error"], json!(true));
    assert_eq!(results[0]["content"][0]["text"], json!("out-tu_1"));
}

#[tokio::test]
async fn truncated_tool_input_surfaces_verbatim() {
    let script = ScriptMessage {
        blocks: vec![ScriptBlock::Tool {
            id: "tu_1".to_string(),
            name: "shell".to_string(),
            chunks: vec![r#"{"command":"ls"#.to_string()],
        }],
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let mut req = request("claude-x", user_only());
    req.tools = vec![shell_spec()];

    let (result, _) = one_turn(&mock, 1000, req).await;
    let result = result.expect("adapter must not fail on bad tool JSON (the runner rejects it)");
    assert_eq!(result.tool_calls.len(), 1);
    assert!(
        serde_json::from_str::<Value>(&result.tool_calls[0].arguments).is_err(),
        "truncated input must reach the runner as invalid JSON: {:?}",
        result.tool_calls
    );
}

#[tokio::test]
async fn multi_delta_and_empty_tool_input() {
    let scripts = [
        ScriptMessage {
            blocks: vec![
                ScriptBlock::Tool {
                    id: "tu_1".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![
                        r#"{"com"#.to_string(),
                        r#"mand":"l"#.to_string(),
                        r#"s"}"#.to_string(),
                    ],
                },
                ScriptBlock::Tool {
                    id: "tu_2".to_string(),
                    name: "noargs".to_string(),
                    chunks: Vec::new(),
                },
            ],
            ..ScriptMessage::default()
        },
        ScriptMessage::text("done"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();

    let mut req = request("claude-x", user_only());
    req.tools = vec![shell_spec()];
    let (result, _) = run_turn(&provider, req).await;
    let result = result.expect("turn");
    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(result.tool_calls[0].arguments, r#"{"command":"ls"}"#);
    assert_eq!(result.tool_calls[1].arguments, "{}");

    let mut history = user_only();
    history.push(assistant_from(&result));
    for call in &result.tool_calls {
        history.push(tool_result(call, "out", false));
    }
    let mut req = request("claude-x", history);
    req.tools = vec![shell_spec()];
    run_turn(&provider, req).await.0.expect("second turn");

    let content = mock.request(1).json()["messages"][1]["content"].clone();
    let content = content.as_array().expect("assistant content");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], json!("tool_use"));
    assert_eq!(content[0]["id"], json!("tu_1"));
    assert_eq!(content[0]["input"], json!({"command": "ls"}));
    assert_eq!(content[1]["id"], json!("tu_2"));
    assert_eq!(content[1]["input"], json!({}));
}

#[tokio::test]
async fn empty_end_turn_is_never_replayed() {
    let scripts = [
        ScriptMessage {
            blocks: Vec::new(),
            ..ScriptMessage::default()
        },
        ScriptMessage::text("second answer"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();

    let mut history = vec![subagent_mcp::provider::Message {
        role: Role::User,
        text: "first".to_string(),
        ..Default::default()
    }];
    let (result, _) = run_turn(&provider, request("claude-x", history.clone())).await;
    let result = result.expect("empty end_turn must be accepted as a final answer");
    assert_eq!(result.text, "");
    assert!(result.tool_calls.is_empty());
    assert_eq!(
        mock.request(0).json()["tools"],
        json!([]),
        "tools is emitted even when empty (Go's non-nil slice)"
    );

    history.push(assistant_from(&result));
    history.push(subagent_mcp::provider::Message {
        role: Role::User,
        text: "second".to_string(),
        ..Default::default()
    });
    let (second, _) = run_turn(&provider, request("claude-x", history)).await;
    assert_eq!(second.expect("second turn").text, "second answer");

    for (index, entry) in mock.request(1).json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .enumerate()
    {
        if entry["role"] != json!("assistant") {
            continue;
        }
        let content = entry["content"].as_array().expect("content");
        assert!(
            !content.is_empty(),
            "request 1 message {index} replays an empty assistant message: {entry}"
        );
        for block in content {
            assert!(
                !(block["type"] == json!("text") && block["text"] == json!("")),
                "request 1 message {index} replays an empty text block: {block}"
            );
        }
    }
}

#[tokio::test]
async fn corrupt_replay_payload_is_reported() {
    let mock = Mock::start(Vec::new()).await;
    let history = vec![
        subagent_mcp::provider::Message {
            role: Role::User,
            text: "hi".to_string(),
            ..Default::default()
        },
        subagent_mcp::provider::Message {
            role: Role::Assistant,
            // Valid JSON, but not the stored turn array the adapter wrote.
            opaque: Some(serde_json::value::to_raw_value(&json!({"a": 1})).unwrap()),
            ..Default::default()
        },
    ];
    let (result, _) = one_turn(&mock, 1000, request("claude-x", history)).await;
    let error = result.expect_err("corrupt replay payload");
    assert!(
        error
            .message
            .starts_with("messages: corrupt replay payload: "),
        "unexpected error: {:?}",
        error.message
    );
    assert_eq!(mock.request_count(), 0, "no request may be sent");
}

// ---------------------------------------------------------------------------
// Stop reasons and errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_reasons_fail_the_turn() {
    let refusal = ScriptMessage {
        blocks: vec![ScriptBlock::Text("no".to_string())],
        stop_reason: Some("refusal".to_string()),
        refusal_category: Some("cyber".to_string()),
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![refusal.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("refusal").message,
        "messages: model refused the request (category \"cyber\"): scripted refusal"
    );

    let max_tokens = ScriptMessage {
        blocks: vec![ScriptBlock::Text("cut".to_string())],
        stop_reason: Some("max_tokens".to_string()),
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![max_tokens.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("max_tokens").message,
        "messages: response hit max_tokens; raise max_output_tokens in the provider config"
    );

    let mock = Mock::start(vec![ScriptMessage::http_error(400).scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let error = result.expect_err("HTTP 400");
    assert!(
        error.message.contains("scripted failure"),
        "HTTP error text: {:?}",
        error.message
    );
}

#[tokio::test]
async fn context_window_exceeded_fails() {
    let script = ScriptMessage {
        blocks: vec![ScriptBlock::Text("overflow".to_string())],
        stop_reason: Some("model_context_window_exceeded".to_string()),
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("context window").message,
        "messages: response hit model_context_window_exceeded; shorten the conversation or use a model with a larger context window"
    );
}

#[tokio::test]
async fn truncated_stream_fails() {
    for (cut, want_deltas) in [(Truncate::Delta, "half an answer"), (Truncate::Block, "")] {
        let script = ScriptMessage {
            blocks: vec![ScriptBlock::Text("half an answer".to_string())],
            truncate: Some(cut),
            ..ScriptMessage::default()
        };
        let mock = Mock::start(vec![script.scripted()]).await;
        let (result, deltas) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
        let error = result.expect_err("truncated stream must fail the turn");
        assert!(
            error.message.contains("stop signal"),
            "want a missing-stop-signal error, got {:?}",
            error.message
        );
        assert_eq!(deltas, want_deltas);
        if cut == Truncate::Delta {
            assert!(
                error.message.contains("half an answer"),
                "partial text dropped from the error: {:?}",
                error.message
            );
            assert_eq!(
                error.message,
                "messages: stream ended without a terminal stop signal (partial text: \"half an answer\")"
            );
        }
    }
}

#[tokio::test]
async fn pause_turn_continues() {
    let scripts = [
        ScriptMessage {
            blocks: vec![ScriptBlock::Text("part one ".to_string())],
            stop_reason: Some("pause_turn".to_string()),
            ..ScriptMessage::default()
        },
        ScriptMessage::text("part two"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let result = result.expect("pause_turn continuation");
    assert!(result.text.contains("part one"));
    assert!(result.text.contains("part two"));

    let second = mock.request(1).json()["messages"].clone();
    let second = second.as_array().expect("messages");
    assert_eq!(second.len(), 2);
    assert_eq!(second[1]["role"], json!("assistant"));
    assert_eq!(second[1]["content"][0]["text"], json!("part one "));
}

#[tokio::test]
async fn pause_turn_exhausts_continuations() {
    let scripts: Vec<Scripted> = (0..=MAX_PAUSE_CONTINUATIONS)
        .map(|index| {
            ScriptMessage {
                blocks: vec![ScriptBlock::Text(format!("part {index} "))],
                stop_reason: Some("pause_turn".to_string()),
                ..ScriptMessage::default()
            }
            .scripted()
        })
        .collect();
    let mock = Mock::start(scripts).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("still paused").message,
        "messages: turn still paused after 5 continuations"
    );
    assert_eq!(mock.request_count(), MAX_PAUSE_CONTINUATIONS + 1);
    assert_eq!(mock.unexpected(), 0);
}

// ---------------------------------------------------------------------------
// Usage mapping: input includes cache, cached is a subset, total = input+output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_mapping_is_one_meaning() {
    let script = ScriptMessage {
        blocks: vec![ScriptBlock::Text("hi".to_string())],
        input_tokens: 25,
        cache_read_tokens: 50,
        cache_creation_tokens: 25,
        output_tokens: 20,
        thinking_tokens: 7,
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let usage = result.expect("turn").usage.expect("usage");
    assert_eq!(
        usage,
        Usage {
            input: 100,
            cached: 50,
            output: 20,
            reasoning: 7,
            total: 120,
        }
    );
    assert!(usage.cached <= usage.input, "cached is a subset of input");
    assert_eq!(usage.total, usage.input + usage.output);
}

// ---------------------------------------------------------------------------
// Model listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_models_returns_ids() {
    let mock = Mock::start(vec![Scripted::json(
        br#"{"data":[{"id":"model-a","type":"model"},{"id":"model-b","type":"model"},{"id":"model-c","type":"model"}],"has_more":false}"#.to_vec(),
    )])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let ids = provider
        .list_models()
        .await
        .expect("list models")
        .into_iter()
        .map(|info| info.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["model-a", "model-b", "model-c"]);

    let recorded = mock.request(0);
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.path, "/v1/models");
    assert_eq!(recorded.header("x-api-key"), Some("k"));
}

#[tokio::test]
async fn list_models_follows_pagination() {
    let mock = Mock::start(vec![
        Scripted::json(
            br#"{"data":[{"id":"model-a"}],"has_more":true,"last_id":"model-a"}"#.to_vec(),
        ),
        Scripted::json(br#"{"data":[{"id":"model-b"}],"has_more":false}"#.to_vec()),
    ])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let ids = provider
        .list_models()
        .await
        .expect("list models")
        .into_iter()
        .map(|info| info.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["model-a", "model-b"]);
    assert_eq!(mock.request(1).path, "/v1/models?after_id=model-a");
}

#[tokio::test]
async fn list_models_http_error() {
    let mock = Mock::start(vec![Scripted::status(
        401,
        br#"{"type":"error","error":{"type":"authentication_error","message":"scripted failure"}}"#
            .to_vec(),
    )])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let error = provider.list_models().await.expect_err("HTTP error");
    assert!(
        error.message.contains("list models"),
        "want wrapped \"list models\" context, got {:?}",
        error.message
    );
    assert!(
        error.message.contains("scripted failure"),
        "want the API message, got {:?}",
        error.message
    );
    assert_eq!(mock.request_count(), 1, "a 401 must not be retried");
}

#[tokio::test]
async fn missing_env_key_is_reported() {
    let mock = Mock::start(Vec::new()).await;
    let mut cfg = mock.config(1000);
    cfg.key = None;
    cfg.env_key = Some("SUBAGENT_MCP_TEST_MESSAGES_KEY_UNSET".to_string());
    let error = match Messages::new("anthropic", &cfg) {
        Ok(_) => panic!("a missing env key must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error.message,
        "environment variable SUBAGENT_MCP_TEST_MESSAGES_KEY_UNSET (providers.anthropic.env_key) must be set to the API key"
    );
}

// ---------------------------------------------------------------------------
// Shared pool: one connection per base URL, one key per session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_sessions_share_one_connection() {
    let mock = Mock::start(vec![
        ScriptMessage::text("first").scripted(),
        ScriptMessage::text("second").scripted(),
    ])
    .await;

    let mut first_cfg = mock.config(1000);
    first_cfg.key = Some("key-first".to_string());
    let first = Messages::new("session-1", &first_cfg).unwrap();
    let mut second_cfg = mock.config(1000);
    second_cfg.key = Some("key-second".to_string());
    let second = Messages::new("session-2", &second_cfg).unwrap();

    assert_eq!(first.name(), "session-1");
    assert_eq!(second.name(), "session-2");

    let (result, _) = run_turn(&first, request("model-1", user_only())).await;
    assert_eq!(result.expect("first turn").text, "first");
    let (result, _) = run_turn(&second, request("model-2", user_only())).await;
    assert_eq!(result.expect("second turn").text, "second");

    assert_eq!(
        mock.connections(),
        1,
        "two sequential sessions must reuse the endpoint's pool"
    );
    assert_eq!(mock.request(0).header("x-api-key"), Some("key-first"));
    assert_eq!(mock.request(1).header("x-api-key"), Some("key-second"));
}

// ---------------------------------------------------------------------------
// extra_body comes from the config (PooledClient::from_config)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_body_is_applied_from_config() {
    let mock = Mock::start(vec![ScriptMessage::text("ok").scripted()]).await;
    let mut cfg = mock.config(1000);
    let mut extra = serde_json::Map::new();
    extra.insert("custom".to_string(), json!({"k": "v"}));
    cfg.extra_body = Some(extra);

    let provider = Messages::new("anthropic", &cfg).unwrap();
    run_turn(&provider, request("claude-x", user_only()))
        .await
        .0
        .expect("turn");

    let body = mock.request(0).json();
    assert_eq!(body["custom"], json!({"k": "v"}));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["model"], json!("claude-x"));
}

// ---------------------------------------------------------------------------
// Messages-specific SSE framing (fixtures under tests/fixtures/sse/messages)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blank_line_dispatch_skips_untyped_empty_events() {
    let (result, deltas) = run_fixture("blank_lines_and_empty_events.sse").await;
    let result = result.expect("untyped empty events are dispatched and ignored");
    assert_eq!(result.text, "blank");
    assert_eq!(deltas, "blank");
}

#[tokio::test]
async fn multiple_data_lines_concatenate_with_newline() {
    let (result, _) = run_fixture("split_data_lines.sse").await;
    assert_eq!(result.expect("split data lines").text, "split");

    let (result, _) = run_fixture("two_data_lines_invalid.sse").await;
    assert!(
        result.is_err(),
        "concatenated data lines must be decoded as one JSON document and fail"
    );
}

#[tokio::test]
async fn done_is_ordinary_data() {
    let (result, _) = run_fixture("done_as_data.sse").await;
    assert!(
        result.is_err(),
        "[DONE] is ordinary data: decoding it as an event must fail"
    );
}

#[tokio::test]
async fn empty_event_with_a_known_type_is_dispatched() {
    let (result, _) = run_fixture("empty_event_dispatched.sse").await;
    assert!(
        result.is_err(),
        "an empty event must be dispatched and fail JSON decoding"
    );
}

#[tokio::test]
async fn eof_without_message_delta_fails() {
    let (result, _) = run_fixture("eof_without_message_delta.sse").await;
    let error = result.expect_err("EOF without message_delta must fail");
    assert!(
        error.message.contains("stop signal"),
        "want a missing-stop-signal error, got {:?}",
        error.message
    );
    assert!(
        error.message.contains("partial"),
        "want the streamed partial text, got {:?}",
        error.message
    );
}

#[tokio::test]
async fn eof_with_stop_reason_without_message_stop_succeeds() {
    let (result, _) = run_fixture("stop_reason_without_message_stop.sse").await;
    let result = result.expect("EOF with a non-empty stop_reason is a complete turn");
    assert_eq!(result.text, "done");
}

#[tokio::test]
async fn stream_error_event_fails() {
    let (result, _) = run_fixture("error_event.sse").await;
    let error = result.expect_err("an error event must fail the turn");
    assert!(
        error.message.contains("upstream overloaded"),
        "want the streamed error body, got {:?}",
        error.message
    );
}

#[tokio::test]
async fn line_longer_than_32_mib_fails() {
    const CAP: usize = 32 * 1024 * 1024;
    let mut body = b"event: message_start\ndata: ".to_vec();
    body.extend(std::iter::repeat_n(b'x', CAP + 1));
    body.extend_from_slice(b"\n\n");
    let mock = Mock::start(vec![Scripted::sse(body)]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let error = result.expect_err("a line over the 32 MiB cap must fail");
    assert!(
        error.message.contains("token too long"),
        "want a scanner cap error, got {:?}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// Opaque payload keeps the streamed message JSON
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opaque_payload_keeps_the_streamed_message_fields() {
    let mock = Mock::start(vec![ScriptMessage::text("hi").scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let opaque = result.expect("turn").opaque.expect("opaque payload");
    let turn: Value = serde_json::from_str(opaque.get()).expect("opaque is JSON");
    let message = &turn[0];

    // Everything the stream sent is preserved and refreshed.
    assert_eq!(message["id"], json!("msg_0"));
    assert_eq!(message["type"], json!("message"));
    assert_eq!(message["model"], json!("claude-x"));
    assert_eq!(message["role"], json!("assistant"));
    assert!(message.get("stop_sequence").is_some());
    assert_eq!(message["stop_reason"], json!("end_turn"));
    assert_eq!(message["content"][0]["text"], json!("hi"));
    assert_eq!(message["usage"]["output_tokens"], json!(0));

    // Go re-marshals the typed SDK `Message`, which adds zero-valued required
    // fields (`container`, `stop_details`) and expands `usage`; we keep the
    // server's message JSON. The measured byte delta is recorded in the task
    // report (745 B for a thinking+text turn, 318 B for an empty end_turn).
    assert!(
        message.get("container").is_none(),
        "the compact server message JSON is the stored shape"
    );
}

// ---------------------------------------------------------------------------
// SDK retry policy (Go `MaxRetries: 2`, i.e. three attempts)
// ---------------------------------------------------------------------------

fn no_wait() -> RetryDelay {
    Arc::new(|_, _| Duration::ZERO)
}

/// Retry delays the seam was asked to apply: `(attempt, Retry-After)`.
type RecordedDelays = Arc<Mutex<Vec<(usize, Option<Duration>)>>>;

#[tokio::test]
async fn retries_retryable_statuses_then_succeeds() {
    let mock = Mock::start(vec![
        ScriptMessage::http_error(500).scripted(),
        ScriptMessage::http_error(503).scripted(),
        ScriptMessage::text("recovered").scripted(),
    ])
    .await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(no_wait());

    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    assert_eq!(result.expect("turn after retries").text, "recovered");
    assert_eq!(mock.request_count(), MAX_RETRIES + 1);
    assert_eq!(mock.unexpected(), 0);
}

#[tokio::test]
async fn retries_stop_after_max_retries() {
    let mock = Mock::start(vec![
        ScriptMessage::http_error(500).scripted(),
        ScriptMessage::http_error(500).scripted(),
        ScriptMessage::http_error(500).scripted(),
    ])
    .await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(no_wait());

    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    let error = result.expect_err("retries exhausted");
    assert!(
        error.message.contains("scripted failure"),
        "want the final API error, got {:?}",
        error.message
    );
    assert_eq!(mock.request_count(), MAX_RETRIES + 1);
    assert_eq!(mock.unexpected(), 0);
}

#[tokio::test]
async fn does_not_retry_client_errors() {
    let mock = Mock::start(vec![ScriptMessage::http_error(400).scripted()]).await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(no_wait());

    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    let error = result.expect_err("400 is not retryable");
    assert!(error.message.contains("scripted failure"));
    assert_eq!(mock.request_count(), 1, "a 400 must not be retried");
    assert_eq!(mock.unexpected(), 0);
}

#[tokio::test]
async fn retries_connection_errors() {
    let mock = Mock::start(vec![
        Scripted::drop_connection(),
        ScriptMessage::text("reconnected").scripted(),
    ])
    .await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(no_wait());

    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    assert_eq!(result.expect("connection retry").text, "reconnected");
    assert_eq!(mock.request_count(), 2);
}

#[tokio::test]
async fn honors_retry_after_header() {
    // The retry delay seam records what the SDK-style policy computed. The
    // server's Retry-After-Ms must win over the ~500 ms exponential default.
    let mock = Mock::start(vec![
        ScriptMessage::http_error(429)
            .scripted()
            .with_headers(&[("Retry-After-Ms", "0")]),
        ScriptMessage::text("after header").scripted(),
    ])
    .await;

    let recorded: RecordedDelays = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&recorded);
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(Arc::new(move |attempt, retry_after| {
        recorder.lock().unwrap().push((attempt, retry_after));
        Duration::ZERO
    }));

    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    assert_eq!(result.expect("429 retry").text, "after header");
    assert_eq!(mock.request_count(), 2);
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![(0, Some(Duration::ZERO))],
        "the 429 Retry-After-Ms header must be parsed into the retry delay"
    );
}

#[tokio::test]
async fn base_url_path_prefix_is_kept_on_the_wire() {
    // Go's `WithBaseURL` appends `/` to a non-empty path before resolving the
    // request path, so `https://gw/anthropic` + `v1/messages` keeps the prefix.
    for (suffix, expected) in [
        ("/anthropic", "/anthropic/v1/messages"),
        ("/anthropic/", "/anthropic/v1/messages"),
    ] {
        let mock = Mock::start(vec![ScriptMessage::text("prefixed").scripted()]).await;
        let mut cfg = mock.config(1000);
        cfg.base_url = format!("{}{suffix}", mock.base);
        let provider = Messages::new("anthropic", &cfg).unwrap();

        let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
        assert_eq!(
            result.expect("prefixed base URL turn").text,
            "prefixed",
            "{suffix}"
        );
        assert_eq!(mock.request(0).path, expected, "{suffix}");
    }
}

#[tokio::test]
async fn invalid_retry_after_headers_clamp_to_zero_like_go() {
    for (name, value) in [
        ("Retry-After-Ms", "-1"),
        ("Retry-After-Ms", "NaN"),
        ("Retry-After-Ms", "1e13"),
        ("Retry-After", "inf"),
        ("Retry-After", "1e10"),
        ("Retry-After", "1e300"),
    ] {
        let mock = Mock::start(vec![
            ScriptMessage::http_error(429)
                .scripted()
                .with_headers(&[(name, value)]),
            ScriptMessage::text("after clamp").scripted(),
        ])
        .await;

        let recorded: RecordedDelays = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&recorded);
        let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
        provider.set_retry_delay(Arc::new(move |attempt, retry_after| {
            recorder.lock().unwrap().push((attempt, retry_after));
            Duration::ZERO
        }));

        let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
        assert_eq!(
            result.expect("invalid Retry-After must not panic").text,
            "after clamp",
            "{name}: {value}"
        );
        assert_eq!(mock.request_count(), 2, "{name}: {value}");
        assert_eq!(
            *recorded.lock().unwrap(),
            vec![(0, Some(Duration::ZERO))],
            "{name}: {value} must clamp to an immediate retry like Go"
        );
    }
}

#[tokio::test]
async fn retry_does_not_read_a_stalled_error_body() {
    // A retryable error whose body never finishes. Go closes the body unread
    // before retrying; reading it here would block the turn on the stalled
    // body and mask the retry delay (this is how `honors_retry_after_header`
    // regressed in the integration merge). The timeout keeps a regression from
    // hanging the suite for the mock's full stall.
    let mock = Mock::start(vec![
        Scripted::stall_status(
            500,
            br#"{"type":"error","error":{"type":"overloaded_error","message":"slow"}}"#.to_vec(),
        ),
        ScriptMessage::text("after stall").scripted(),
    ])
    .await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(no_wait());

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_turn(&provider, request("claude-x", user_only())),
    )
    .await;
    let (result, _) = outcome.expect("the retry blocked on the retryable error body");
    assert_eq!(
        result.expect("retry past a stalled error body").text,
        "after stall"
    );
    assert_eq!(mock.request_count(), 2);
}

#[tokio::test]
async fn cancellation_is_not_retried() {
    let mock = Mock::start(vec![
        ScriptMessage::http_error(500).scripted(),
        ScriptMessage::text("unused").scripted(),
    ])
    .await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let token = CancellationToken::new();
    let cancel = token.clone();
    provider.set_retry_delay(Arc::new(move |_, _| {
        cancel.cancel();
        Duration::ZERO
    }));

    let ctx = RunContext {
        cancel: token,
        deadline: None,
    };
    let result = provider
        .turn(
            ctx,
            request("claude-x", user_only()),
            Box::new(|_: String| {}),
        )
        .await;
    assert_eq!(result.expect_err("canceled").message, "context canceled");
    assert_eq!(mock.request_count(), 1, "a cancelled turn must not retry");
}

#[tokio::test]
async fn deadline_is_not_retried() {
    let mock = Mock::start(vec![
        ScriptMessage::http_error(500).scripted(),
        ScriptMessage::text("unused").scripted(),
    ])
    .await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(Arc::new(|_, _| Duration::from_millis(200)));

    let ctx = RunContext {
        cancel: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() + Duration::from_millis(50)),
    };
    let result = provider
        .turn(
            ctx,
            request("claude-x", user_only()),
            Box::new(|_: String| {}),
        )
        .await;
    assert_eq!(
        result.expect_err("deadline").message,
        "context deadline exceeded"
    );
    assert_eq!(
        mock.request_count(),
        1,
        "a turn past its deadline must not retry"
    );
}

#[tokio::test]
async fn list_models_retries_retryable_statuses() {
    let mock = Mock::start(vec![
        Scripted::status(
            503,
            br#"{"type":"error","error":{"type":"overloaded_error","message":"try later"}}"#
                .to_vec(),
        ),
        Scripted::json(br#"{"data":[{"id":"model-a"}],"has_more":false}"#.to_vec()),
    ])
    .await;
    let mut provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    provider.set_retry_delay(no_wait());

    let ids = provider
        .list_models()
        .await
        .expect("list models after retry")
        .into_iter()
        .map(|info| info.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["model-a"]);
    assert_eq!(mock.request_count(), 2);
}

// ---------------------------------------------------------------------------
// Concurrency: one pool per endpoint, one key per session
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_keep_keys_on_shared_pool() {
    const SESSIONS: usize = 24;
    let scripts: Vec<Scripted> = (0..SESSIONS / 2)
        .map(|_| ScriptMessage::text("ok").scripted())
        .collect();
    let first = Mock::start(scripts.clone()).await;
    let second = Mock::start(scripts).await;

    let mut handles = Vec::new();
    for index in 0..SESSIONS {
        let mut cfg = if index % 2 == 0 {
            first.config(1000)
        } else {
            second.config(1000)
        };
        cfg.key = Some(format!("key-{index:02}"));
        handles.push(tokio::spawn(async move {
            let provider = Messages::new(&format!("session-{index}"), &cfg).unwrap();
            let (result, _) =
                run_turn(&provider, request(&format!("model-{index}"), user_only())).await;
            result.expect("concurrent turn").text
        }));
    }
    for handle in handles {
        assert_eq!(handle.await.expect("join"), "ok");
    }

    for mock in [&first, &second] {
        assert_eq!(mock.request_count(), SESSIONS / 2);
        for index in 0..mock.request_count() {
            let recorded = mock.request(index);
            let model = recorded.json()["model"].as_str().unwrap().to_string();
            let session: usize = model.trim_start_matches("model-").parse().unwrap();
            assert_eq!(
                recorded.header("x-api-key"),
                Some(format!("key-{session:02}").as_str()),
                "{model} carried another session's key"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Redacted thinking blocks survive opaque replay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn redacted_thinking_is_preserved_on_replay() {
    let scripts = [
        ScriptMessage {
            blocks: vec![
                ScriptBlock::Redacted {
                    data: "opaque-redacted".to_string(),
                },
                ScriptBlock::Text("done".to_string()),
            ],
            ..ScriptMessage::default()
        },
        ScriptMessage::text("second"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();

    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    let result = result.expect("redacted turn");
    assert_eq!(result.text, "done");

    let mut history = user_only();
    history.push(assistant_from(&result));
    run_turn(&provider, request("claude-x", history))
        .await
        .0
        .expect("replay turn");

    let content = mock.request(1).json()["messages"][1]["content"].clone();
    let content = content.as_array().expect("assistant content");
    assert_eq!(content[0]["type"], json!("redacted_thinking"));
    assert_eq!(content[0]["data"], json!("opaque-redacted"));
}

// ---------------------------------------------------------------------------
// Cancellation and the run deadline stop a waiting stream
// ---------------------------------------------------------------------------

async fn wait_for_request(mock: &Mock) {
    for _ in 0..400 {
        if mock.request_count() > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the mock never received the request");
}

#[tokio::test]
async fn cancellation_stops_a_waiting_stream() {
    let mock = Mock::start(vec![Scripted::stall(
        b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n".to_vec(),
    )])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let token = CancellationToken::new();
    let cancel = token.clone();
    let handle = tokio::spawn(async move {
        provider
            .turn(
                RunContext {
                    cancel: token,
                    deadline: None,
                },
                request("claude-x", user_only()),
                Box::new(|_: String| {}),
            )
            .await
    });

    wait_for_request(&mock).await;
    cancel.cancel();
    let error = handle.await.expect("join").expect_err("canceled stream");
    assert_eq!(error.message, "context canceled");
}

#[tokio::test]
async fn deadline_stops_a_waiting_stream() {
    let mock = Mock::start(vec![Scripted::stall(
        b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n".to_vec(),
    )])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let result = provider
        .turn(
            RunContext {
                cancel: CancellationToken::new(),
                deadline: Some(tokio::time::Instant::now() + Duration::from_millis(100)),
            },
            request("claude-x", user_only()),
            Box::new(|_: String| {}),
        )
        .await;
    assert_eq!(
        result.expect_err("deadline").message,
        "context deadline exceeded"
    );
}
