//! Task 11: Responses adapter wire tests.
//!
//! Ports `internal/provider/responses/{adapter,shared_transport}_test.go` and
//! adds the SSE framing and OpenAI SDK retry cases the Rust adapter owns:
//! blank-line dispatch, comment/empty-event skipping, `data:` line joining,
//! the 32 MiB line cap (both sides of the boundary), EOF without a trailing
//! blank line, `[DONE]` ending the stream without counting as a terminal
//! response event, and the SDK's `MaxRetries: 2` policy for 408/409/429/5xx,
//! connection errors, `x-should-retry`, and `Retry-After` hints.
//!
//! The credential-environment test lives in its own binary
//! (`wire_responses_env.rs`) so `set_var` cannot race another test's
//! `PooledClient` environment read.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::value::RawValue;
use serde_json::{Value, json};
use subagent_mcp::provider::responses::{
    MAX_ATTEMPTS, MAX_RETRY_DELAY, Responses, default_retry_delay,
};
use subagent_mcp::provider::sse::SSE_MAX_LINE;
use subagent_mcp::provider::{
    DeltaSink, Message, ModelLister, Provider, Role, ToolSpec, TurnRequest, Usage,
};
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;

#[path = "support/responses_fake.rs"]
mod fake;

use fake::*;

// ---------------------------------------------------------------------------
// Ported Go adapter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_turn_sends_the_full_request_and_reports_usage() {
    let fake = spawn_fake(vec![Scripted::Turn(TurnScript {
        items: vec![Item::text("hello")],
        input_tokens: 10,
        cached_tokens: 4,
        output_tokens: 3,
        reasoning_tokens: 1,
        ..Default::default()
    })])
    .await;
    let provider = adapter(&fake, "k");

    let deltas = Arc::new(Mutex::new(String::new()));
    let sink: DeltaSink = {
        let deltas = Arc::clone(&deltas);
        Box::new(move |delta| deltas.lock().unwrap().push_str(&delta))
    };
    let result = provider
        .turn(
            context(),
            TurnRequest {
                model: "gpt-x".to_string(),
                effort: Some("high".to_string()),
                system: "sys".to_string(),
                messages: vec![Message {
                    role: Role::User,
                    text: "hi".to_string(),
                    ..Default::default()
                }],
                tools: vec![shell_spec()],
            },
            sink,
        )
        .await
        .unwrap();

    assert_eq!(result.text, "hello");
    assert_eq!(*deltas.lock().unwrap(), "hello");
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 10,
            cached: 4,
            output: 3,
            reasoning: 1,
            total: 13,
        })
    );

    let body = fake.request(0);
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["instructions"], json!("sys"));
    assert_eq!(body["model"], json!("gpt-x"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(body["input"], json!([{"content": "hi", "role": "user"}]));
    assert_eq!(
        body["max_output_tokens"],
        Value::Null,
        "an unset limit must stay off the wire"
    );

    let tool = &body["tools"][0];
    assert_eq!(tool["type"], json!("function"));
    assert_eq!(tool["name"], json!("shell"));
    assert_eq!(tool["description"], json!("run"));
    assert_eq!(tool["strict"], json!(false));
    assert!(tool["parameters"].is_object(), "raw schema must be sent");

    let headers = fake.headers(0);
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer k")
    );
    assert!(
        headers.get("accept").is_some_and(|value| !value.is_empty()),
        "standard Accept header missing"
    );
    assert_eq!(
        fake.method_and_path(0),
        ("POST".to_string(), "/responses".to_string())
    );
}

#[tokio::test]
async fn replays_every_turn_with_opaque_items() {
    let fake = spawn_fake(vec![
        Scripted::Turn(TurnScript {
            items: vec![
                Item::reasoning("plan 1", "enc-1"),
                Item::call("call_1", "shell", r#"{"command":"ls"}"#),
                Item::call("call_2", "shell", r#"{"command":"pwd"}"#),
            ],
            ..Default::default()
        }),
        Scripted::Turn(TurnScript {
            items: vec![
                Item::reasoning("plan 2", "enc-2"),
                Item::call("call_3", "shell", r#"{"command":"cat a"}"#),
            ],
            ..Default::default()
        }),
        text_turn("done"),
    ])
    .await;
    let provider = adapter(&fake, "k");

    let mut history = vec![Message {
        role: Role::User,
        text: "go".to_string(),
        ..Default::default()
    }];
    for turn in 0..3 {
        let result = run_turn(
            &provider,
            TurnRequest {
                model: "gpt-x".to_string(),
                messages: history.clone(),
                tools: vec![shell_spec()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if turn == 0 {
            assert_eq!(result.reasoning.as_deref(), Some("plan 1"));
            assert_eq!(result.tool_calls.len(), 2);
            assert_eq!(result.tool_calls[1].id, "call_2");
            assert_eq!(result.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        }
        history.push(Message {
            role: Role::Assistant,
            text: result.text.clone(),
            tool_calls: result.tool_calls.clone(),
            opaque: result.opaque.clone(),
            ..Default::default()
        });
        for call in &result.tool_calls {
            history.push(Message {
                role: Role::Tool,
                tool_call_id: Some(call.id.clone()),
                text: format!("out-{}", call.id),
                ..Default::default()
            });
        }
    }

    let body = fake.request(2);
    let input = body["input"].as_array().unwrap();
    let kinds: Vec<String> = input.iter().map(item_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "message:user",
            "reasoning",
            "function_call",
            "function_call",
            "function_call_output",
            "function_call_output",
            "reasoning",
            "function_call",
            "function_call_output",
        ]
    );
    assert_eq!(input[1]["encrypted_content"], json!("enc-1"));
    assert_eq!(input[6]["encrypted_content"], json!("enc-2"));
    assert_eq!(input[4]["call_id"], json!("call_1"));
    assert_eq!(input[4]["output"], json!("out-call_1"));
    assert_eq!(input[4]["type"], json!("function_call_output"));
}

/// An assistant turn's opaque items are replayed as the exact bytes the
/// terminal response carried, not re-encoded from a parsed value.
#[tokio::test]
async fn assistant_opaque_replays_item_bytes_verbatim() {
    const RAW: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":0,\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"enc-1\",\"id\":\"rs_0\",\"summary\":[{\"text\":\"p\",\"type\":\"summary_text\"}]}],\"usage\":{}}}\n\n";
    let fake = spawn_fake(vec![Scripted::Raw(RAW.to_string()), text_turn("ok")]).await;
    let provider = adapter(&fake, "k");

    let first = run_turn(&provider, request("gpt-x")).await.unwrap();
    assert_eq!(
        first.opaque.as_deref().map(RawValue::get),
        Some(
            r#"[{"type":"reasoning","encrypted_content":"enc-1","id":"rs_0","summary":[{"text":"p","type":"summary_text"}]}]"#
        )
    );

    run_turn(
        &provider,
        TurnRequest {
            model: "gpt-x".to_string(),
            messages: vec![
                Message {
                    role: Role::User,
                    text: "go".to_string(),
                    ..Default::default()
                },
                Message {
                    role: Role::Assistant,
                    opaque: first.opaque.clone(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let raw = String::from_utf8(fake.raw_request(1)).unwrap();
    let item = r#"{"type":"reasoning","encrypted_content":"enc-1","id":"rs_0","summary":[{"text":"p","type":"summary_text"}]}"#;
    assert!(raw.contains(item), "replayed item bytes changed:\n{raw}");
}

#[tokio::test]
async fn incomplete_and_failed_responses_use_the_go_error_text() {
    let incomplete = spawn_fake(vec![Scripted::Turn(TurnScript {
        response_status: Some("incomplete"),
        incomplete_reason: "max_output_tokens".to_string(),
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&incomplete, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: response incomplete: max_output_tokens"
    );

    let failed = spawn_fake(vec![Scripted::Turn(TurnScript {
        response_status: Some("failed"),
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&failed, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: response failed: scripted failure"
    );
}

#[tokio::test]
async fn refusal_part_fails_the_turn() {
    let fake = spawn_fake(vec![Scripted::Turn(TurnScript {
        items: vec![Item::refusal("I cannot help with that.")],
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: model refused: I cannot help with that."
    );
}

#[tokio::test]
async fn stream_error_event_reports_code_and_message() {
    let fake = spawn_fake(vec![Scripted::Turn(TurnScript {
        items: vec![Item::text("partial")],
        stream_error: Some((
            "invalid_request_error".to_string(),
            "scripted stream failure".to_string(),
        )),
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: invalid_request_error: scripted stream failure"
    );
}

#[tokio::test]
async fn http_error_fails_the_turn() {
    let fake = spawn_fake(vec![http_error(400)]).await;
    let error = run_turn(&fast_adapter(&fake), request("gpt-x"))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("400") && error.message.contains("scripted failure"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn corrupt_replay_payload_is_rejected_before_sending() {
    let fake = spawn_fake(Vec::new()).await;
    let error = run_turn(
        &adapter(&fake, "k"),
        TurnRequest {
            model: "gpt-x".to_string(),
            messages: vec![Message {
                role: Role::Assistant,
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        error
            .message
            .starts_with("responses: corrupt replay payload:"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(fake.request_count(), 0, "no request must be sent");
}

#[tokio::test]
async fn a_tool_schema_that_is_not_an_object_is_rejected() {
    let fake = spawn_fake(Vec::new()).await;
    let spec = ToolSpec {
        name: "bad".to_string(),
        description: String::new(),
        parameters: RawValue::from_string("[]".to_string()).unwrap(),
    };
    let error = run_turn(
        &adapter(&fake, "k"),
        TurnRequest {
            model: "gpt-x".to_string(),
            messages: vec![Message {
                role: Role::User,
                text: "hi".to_string(),
                ..Default::default()
            }],
            tools: vec![spec],
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.message,
        "responses: tool bad schema: not a JSON object"
    );
    assert_eq!(fake.request_count(), 0);
}

#[tokio::test]
async fn list_models_returns_ids() {
    let fake = spawn_fake(Vec::new()).await;
    fake.set_models(&["model-a", "model-b", "model-c"]);
    let provider = adapter(&fake, "k");
    let ids: Vec<String> = ModelLister::list_models(&provider)
        .await
        .unwrap()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(ids, vec!["model-a", "model-b", "model-c"]);
    assert_eq!(
        fake.method_and_path(0),
        ("GET".to_string(), "/models".to_string())
    );
    assert_eq!(
        fake.headers(0).get("authorization").map(String::as_str),
        Some("Bearer k")
    );
}

#[tokio::test]
async fn list_models_http_error_is_wrapped_with_list_models() {
    let fake = spawn_fake(Vec::new()).await;
    fake.set_models_status(401);
    let error = ModelLister::list_models(&adapter(&fake, "k"))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("list models") && error.message.contains("401"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(
        fake.request_count(),
        1,
        "401 is not retryable, so the listing must be attempted once"
    );
}

#[tokio::test]
async fn configured_max_output_tokens_reaches_the_wire() {
    for (configure, want) in [
        (Some(4096), Some(json!(4096))),
        (None, None),
        (Some(0), None),
    ] {
        let fake = spawn_fake(vec![text_turn("ok")]).await;
        let mut cfg = provider_config(&fake.base, "k");
        cfg.max_output_tokens = configure;
        let provider = Responses::new("openai", &cfg).unwrap();
        run_turn(&provider, request("m")).await.unwrap();
        let body = fake.request(0);
        match want {
            None => assert_eq!(
                body["max_output_tokens"],
                Value::Null,
                "configure = {configure:?} must leave the field out"
            ),
            Some(value) => assert_eq!(body["max_output_tokens"], value),
        }
    }
}

#[tokio::test]
async fn provider_name_is_the_configured_name() {
    let fake = spawn_fake(Vec::new()).await;
    let provider = Responses::new("mine", &provider_config(&fake.base, "k")).unwrap();
    assert_eq!(provider.name(), "mine");
}

// ---------------------------------------------------------------------------
// OpenAI SDK retry policy (MaxRetries: 2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_statuses_are_attempted_three_times() {
    for status in [408, 409, 429, 500, 503] {
        let fake = spawn_fake(vec![
            http_error(status),
            http_error(status),
            http_error(status),
        ])
        .await;
        let error = run_turn(&fast_adapter(&fake), request("gpt-x"))
            .await
            .unwrap_err();
        assert!(
            error.message.contains(&status.to_string()),
            "status {status}: unexpected error: {}",
            error.message
        );
        assert_eq!(
            fake.request_count(),
            MAX_ATTEMPTS,
            "status {status} must use every retry"
        );
    }
}

#[tokio::test]
async fn non_retryable_statuses_are_attempted_once() {
    for status in [400, 401, 403, 404, 422] {
        let fake = spawn_fake(vec![http_error(status), text_turn("unused")]).await;
        let error = run_turn(&fast_adapter(&fake), request("gpt-x"))
            .await
            .unwrap_err();
        assert!(
            error.message.contains(&status.to_string()),
            "status {status}: unexpected error: {}",
            error.message
        );
        assert_eq!(fake.request_count(), 1, "status {status} must not retry");
    }
}

#[tokio::test]
async fn a_retry_can_recover_the_turn() {
    let fake = spawn_fake(vec![http_error(500), http_error(429), text_turn("ok")]).await;
    let result = run_turn(&fast_adapter(&fake), request("gpt-x"))
        .await
        .unwrap();
    assert_eq!(result.text, "ok");
    assert_eq!(fake.request_count(), MAX_ATTEMPTS);
}

#[tokio::test]
async fn model_listing_uses_the_same_retry_policy() {
    let fake = spawn_fake(Vec::new()).await;
    fake.set_models(&["model-a"]);
    fake.set_models_transient_failures(500, MAX_ATTEMPTS - 1);
    let ids: Vec<String> = ModelLister::list_models(&fast_adapter(&fake))
        .await
        .unwrap()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(ids, vec!["model-a"]);
    assert_eq!(fake.request_count(), MAX_ATTEMPTS);
}

#[tokio::test]
async fn connection_errors_are_retried() {
    let (base, attempts) = spawn_closing_server().await;
    let mut provider = Responses::new("openai", &provider_config(&base, "k")).unwrap();
    provider.set_retry_delay(Arc::new(|_, _| Duration::ZERO));
    let error = run_turn(&provider, request("gpt-x")).await.unwrap_err();
    assert!(!error.message.is_empty());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        MAX_ATTEMPTS,
        "a connection error must use every retry"
    );
}

#[tokio::test]
async fn x_should_retry_true_forces_a_retry() {
    let fake = spawn_fake(vec![
        http_error_with(400, &[("x-should-retry", "true")]),
        http_error_with(400, &[("x-should-retry", "true")]),
        text_turn("ok"),
    ])
    .await;
    let result = run_turn(&fast_adapter(&fake), request("gpt-x"))
        .await
        .unwrap();
    assert_eq!(result.text, "ok");
    assert_eq!(fake.request_count(), MAX_ATTEMPTS);
}

#[tokio::test]
async fn x_should_retry_false_suppresses_a_retry() {
    let fake = spawn_fake(vec![
        http_error_with(500, &[("x-should-retry", "false")]),
        text_turn("unused"),
    ])
    .await;
    let error = run_turn(&fast_adapter(&fake), request("gpt-x"))
        .await
        .unwrap_err();
    assert!(error.message.contains("500"));
    assert_eq!(fake.request_count(), 1);
}

#[tokio::test]
async fn retry_after_hints_reach_the_delay_function() {
    let fake = spawn_fake(vec![
        http_error_with(429, &[("Retry-After-Ms", "1500")]),
        text_turn("ok"),
    ])
    .await;
    let mut provider = adapter(&fake, "k");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    provider.set_retry_delay(Arc::new(move |attempt, retry_after| {
        recorder.lock().unwrap().push((attempt, retry_after));
        Duration::ZERO
    }));
    run_turn(&provider, request("gpt-x")).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(0, Some(Duration::from_millis(1500)))]
    );

    let fake = spawn_fake(vec![
        http_error_with(429, &[("Retry-After", "60")]),
        text_turn("ok"),
    ])
    .await;
    let mut provider = adapter(&fake, "k");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    provider.set_retry_delay(Arc::new(move |attempt, retry_after| {
        recorder.lock().unwrap().push((attempt, retry_after));
        Duration::ZERO
    }));
    run_turn(&provider, request("gpt-x")).await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(0, Some(MAX_RETRY_DELAY))],
        "a server-directed delay is capped at 8s"
    );
}

#[test]
fn default_retry_delay_matches_the_sdk_backoff() {
    // Go: 0.5s * 2^attempt, capped at 8s, minus up to a quarter jitter.
    for (attempt, min_ms, max_ms) in [(0usize, 375u64, 500u64), (1, 750, 1000), (2, 1500, 2000)] {
        let delay = default_retry_delay(attempt, None);
        assert!(
            delay >= Duration::from_millis(min_ms) && delay <= Duration::from_millis(max_ms),
            "attempt {attempt}: {delay:?} outside [{min_ms}ms, {max_ms}ms]"
        );
    }
    assert_eq!(
        default_retry_delay(0, Some(Duration::from_secs(3))),
        Duration::from_secs(3),
        "a server-directed delay wins over the exponential backoff"
    );
    assert_eq!(
        default_retry_delay(0, Some(Duration::from_secs(60))),
        MAX_RETRY_DELAY
    );
    assert_eq!(MAX_ATTEMPTS, 3, "Go MaxRetries: 2 means three attempts");
}

#[tokio::test]
async fn cancellation_during_a_retry_wait_is_not_retried() {
    let fake = spawn_fake(vec![http_error(500), text_turn("unused")]).await;
    let mut provider = adapter(&fake, "k");
    provider.set_retry_delay(Arc::new(|_, _| Duration::from_secs(30)));
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        canceller.cancel();
    });
    let error = provider
        .turn(
            RunContext {
                cancel,
                deadline: None,
            },
            request("gpt-x"),
            Box::new(|_| {}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "context canceled");
    assert_eq!(fake.request_count(), 1, "cancellation must not retry");
}

#[tokio::test]
async fn an_elapsed_deadline_during_a_retry_wait_is_not_retried() {
    let fake = spawn_fake(vec![http_error(500), text_turn("unused")]).await;
    let mut provider = adapter(&fake, "k");
    provider.set_retry_delay(Arc::new(|_, _| Duration::from_secs(30)));
    let error = provider
        .turn(
            RunContext {
                cancel: CancellationToken::new(),
                deadline: Some(tokio::time::Instant::now() + Duration::from_millis(20)),
            },
            request("gpt-x"),
            Box::new(|_| {}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.message, "context deadline exceeded");
    assert_eq!(fake.request_count(), 1, "a deadline must not retry");
}

// ---------------------------------------------------------------------------
// SSE framing
// ---------------------------------------------------------------------------

const TEXT_COMPLETED: &str = include_str!("fixtures/sse/responses/text_completed.sse");
const FRAMING_MULTI_DATA: &str = include_str!("fixtures/sse/responses/framing_multi_data.sse");
const DONE_PREFIX: &str = include_str!("fixtures/sse/responses/done_prefix.sse");
const EOF_WITHOUT_BLANK: &str = include_str!("fixtures/sse/responses/eof_without_blank.sse");
const DATA_JOIN_REQUIRES_NEWLINE: &str =
    include_str!("fixtures/sse/responses/data_join_requires_newline.sse");

#[tokio::test]
async fn terminal_event_then_done_completes_the_turn() {
    let fake = spawn_fake(vec![Scripted::Raw(TEXT_COMPLETED.to_string())]).await;
    let result = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap();
    assert_eq!(result.text, "hello");
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 10,
            cached: 4,
            output: 3,
            reasoning: 1,
            total: 13,
        })
    );
}

#[tokio::test]
async fn blank_line_dispatch_skips_comments_and_empty_events() {
    let fake = spawn_fake(vec![Scripted::Raw(FRAMING_MULTI_DATA.to_string())]).await;
    let deltas = Arc::new(Mutex::new(String::new()));
    let sink: DeltaSink = {
        let deltas = Arc::clone(&deltas);
        Box::new(move |delta| deltas.lock().unwrap().push_str(&delta))
    };
    let result = adapter(&fake, "k")
        .turn(context(), request("gpt-x"), sink)
        .await
        .unwrap();
    assert_eq!(result.text, "hello");
    assert_eq!(
        *deltas.lock().unwrap(),
        "hello",
        "data: lines must be joined with a newline and comments/empty events skipped"
    );
    assert_eq!(result.usage.map(|usage| usage.total), Some(2));
}

/// The fixture splits a JSON number across two `data:` lines. The SDK joins
/// them with `\n`, which makes `1\n2` invalid JSON; an implementation that
/// forgot the newline would parse `12` and complete the turn successfully.
#[tokio::test]
async fn data_lines_are_joined_with_a_newline() {
    let fake = spawn_fake(vec![Scripted::Raw(DATA_JOIN_REQUIRES_NEWLINE.to_string())]).await;
    let deltas = Arc::new(Mutex::new(String::new()));
    let sink: DeltaSink = {
        let deltas = Arc::clone(&deltas);
        Box::new(move |delta| deltas.lock().unwrap().push_str(&delta))
    };
    let result = adapter(&fake, "k")
        .turn(context(), request("gpt-x"), sink)
        .await;
    assert!(
        result.is_err(),
        "the joined payload must be invalid JSON, proving the newline was inserted"
    );
    assert_eq!(*deltas.lock().unwrap(), "");
}

#[tokio::test]
async fn data_beginning_with_done_ends_the_stream_but_is_not_terminal() {
    let fake = spawn_fake(vec![Scripted::Raw(DONE_PREFIX.to_string())]).await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: stream ended without a terminal response event"
    );
}

#[tokio::test]
async fn eof_without_a_trailing_blank_line_drops_the_pending_event() {
    let fake = spawn_fake(vec![Scripted::Raw(EOF_WITHOUT_BLANK.to_string())]).await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: stream ended without a terminal response event"
    );
}

#[tokio::test]
async fn a_line_over_the_32_mib_cap_fails_the_stream() {
    let mut body = String::from("data: {\"type\":\"response.output_text.delta\",\"delta\":\"");
    body.push_str(&"x".repeat(SSE_MAX_LINE + 1));
    body.push_str("\"}\n\n");
    let fake = spawn_fake(vec![Scripted::Raw(body)]).await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("maximum"),
        "unexpected error: {}",
        error.message
    );
}

/// A line of exactly `SSE_MAX_LINE - 1` bytes is the largest the Go scanner
/// accepts; only a line of `SSE_MAX_LINE` or more is rejected.
#[tokio::test]
async fn a_line_at_the_32_mib_cap_minus_one_is_accepted() {
    const PREFIX: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"";
    const SUFFIX: &str = "\"}";
    const TERMINAL: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"total_tokens\":1}}}\n\n";
    let padding = SSE_MAX_LINE - 1 - PREFIX.len() - SUFFIX.len();
    let mut body = String::with_capacity(SSE_MAX_LINE + 512);
    body.push_str(PREFIX);
    body.push_str(&"x".repeat(padding));
    body.push_str(SUFFIX);
    body.push_str("\n\n");
    body.push_str(TERMINAL);

    let fake = spawn_fake(vec![Scripted::Raw(body)]).await;
    let delta_len = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&delta_len);
    let result = adapter(&fake, "k")
        .turn(
            context(),
            request("gpt-x"),
            Box::new(move |delta| *counter.lock().unwrap() += delta.len()),
        )
        .await
        .unwrap();
    assert_eq!(result.text, "");
    assert_eq!(*delta_len.lock().unwrap(), padding);
}

// ---------------------------------------------------------------------------
// SDK top-level error envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_top_level_error_field_ends_the_stream() {
    let fake = spawn_fake(vec![Scripted::Raw(
        "data: {\"type\":\"response.output_text.delta\",\"error\":{\"message\":\"boom\"}}\n\n"
            .to_string(),
    )])
    .await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "received error while streaming: {\"message\":\"boom\"}"
    );
}

/// gjson's `Exists()` is true for an explicit null, so the SDK reports a
/// null error field as an empty-message stream error instead of ignoring it.
#[tokio::test]
async fn a_present_null_error_field_ends_the_stream() {
    let fake = spawn_fake(vec![Scripted::Raw(
        "data: {\"type\":\"response.created\",\"error\":null}\n\n".to_string(),
    )])
    .await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(error.message, "received error while streaming: ");
}

// ---------------------------------------------------------------------------
// Connection sharing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_sessions_share_one_connection_with_their_own_keys() {
    let fake = spawn_fake(vec![text_turn("first"), text_turn("second")]).await;
    let first = adapter(&fake, "key-first");
    let second = adapter(&fake, "key-second");

    let result = run_turn(&first, request("model-1")).await.unwrap();
    assert_eq!(result.text, "first");
    let result = run_turn(&second, request("model-2")).await.unwrap();
    assert_eq!(result.text, "second");

    assert_eq!(
        fake.connections(),
        1,
        "two sequential sessions must reuse the endpoint's pooled connection"
    );
    assert_eq!(
        fake.headers(0).get("authorization").map(String::as_str),
        Some("Bearer key-first")
    );
    assert_eq!(
        fake.headers(1).get("authorization").map(String::as_str),
        Some("Bearer key-second")
    );
}
