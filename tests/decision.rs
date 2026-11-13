//! Port of `internal/decision/client_test.go` (Go subagent-mcp v0.13.1), plus
//! Rust-specific coverage for absent usage and RunContext cancellation.
//!
//! A tiny local HTTP/1.1 server records each request and returns a canned
//! response, standing in for Go's `httptest.Server` / roundTripFunc. The exact
//! subagent-mcp-authored error text is asserted with `assert_eq!`; Go stdlib /
//! SDK text (timeouts) is only checked semantically.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;
use subagent_mcp::config::AutoConfig;
use subagent_mcp::decision::{Candidate, DecisionClient, JevDecision};
use subagent_mcp::util::run::RunContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[path = "support/proxy_isolation.rs"]
mod proxy_isolation;

#[test]
fn decision_proxy_bypasses_loopback() {
    if let Some(requests) = proxy_isolation::run("decision_proxy_bypasses_loopback", "HTTP_PROXY") {
        assert!(
            requests.is_empty(),
            "decision leaked to proxy: {requests:?}"
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (base, _, rx) = spawn_mock(Reply::Response {
                status: 200,
                body: r#"{"answers":{"model":{"type":"choice","choice":"c1","confidence":0.5}}}"#
                    .into(),
            })
            .await;
            let client = make_client(&auto_config(&base));
            let result = client
                .choose(
                    &plain_ctx(),
                    "private task",
                    Path::new("/repo"),
                    &candidates(),
                )
                .await
                .unwrap();
            assert_eq!(result.provider, "alpha");
            assert_eq!(
                recorded(rx).await.header("authorization"),
                Some("Bearer test-key")
            );
        });
}

// ---------------------------------------------------------------------------
// Local HTTP mock
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

enum Reply {
    Response { status: u16, body: String },
    Hang,
}

type MockParts = (String, Arc<AtomicUsize>, oneshot::Receiver<RecordedRequest>);

async fn spawn_mock(reply: Reply) -> MockParts {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{}", addr);
    let connections = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = oneshot::channel();
    let conn_count = connections.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        conn_count.fetch_add(1, Ordering::SeqCst);
        let req = read_http_request(&mut socket).await;
        let _ = tx.send(req);
        match reply {
            Reply::Response { status, body } => {
                let reason = if (200..300).contains(&status) {
                    "OK"
                } else {
                    "Error"
                };
                let response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    reason,
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
            Reply::Hang => {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    });
    (base, connections, rx)
}

/// Await the recorded request with a bound, so a request that is never sent
/// fails the test instead of hanging it forever.
async fn recorded(rx: oneshot::Receiver<RecordedRequest>) -> RecordedRequest {
    match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(req)) => req,
        Ok(Err(_)) => panic!("mock server dropped the request channel"),
        Err(_) => panic!("no request reached the mock server"),
    }
}

fn find_crlfcrlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

async fn read_http_request(socket: &mut TcpStream) -> RecordedRequest {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.unwrap();
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_crlfcrlf(&buf) {
            break pos;
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    while buf.len().saturating_sub(header_end) < content_length {
        let n = socket.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[header_end..buf.len().min(header_end + content_length)].to_vec();
    RecordedRequest {
        method,
        path,
        headers,
        body,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn auto_config(base: &str) -> AutoConfig {
    AutoConfig {
        base_url: base.to_string(),
        url: String::new(),
        env_key: None,
        key: Some("test-key".to_string()),
        model: "jev-latest".to_string(),
        timeout_seconds: 5,
    }
}

fn make_client(cfg: &AutoConfig) -> DecisionClient {
    DecisionClient::new(cfg).unwrap()
}

fn plain_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn candidate_a() -> Candidate {
    Candidate {
        provider: "a".into(),
        model: "m".into(),
        api: String::new(),
        description: String::new(),
    }
}

fn candidates() -> Vec<Candidate> {
    vec![
        Candidate {
            provider: "alpha".into(),
            model: "m-fast".into(),
            api: "chat-completions".into(),
            description: "fast, cheap".into(),
        },
        Candidate {
            provider: "bravo".into(),
            model: "org/model".into(),
            api: "codex-app-server".into(),
            description: String::new(),
        },
    ]
}

/// Port of Go `chooseAgainst`: run `choose` against a canned 200 response and
/// return the decision or the exact error message.
async fn choose_against(response: &str) -> Result<JevDecision, String> {
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 200,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let result = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &candidates())
        .await;
    let _ = recorded(rx).await;
    result.map_err(|e| e.message)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn choose_sends_one_choice_question_and_maps_answer() {
    let response = r#"{
        "model":"jev-1.14.0",
        "answers":{"model":{"type":"choice","choice":"c2","confidence":0.71,
            "probabilities":{"c1":0.29,"c2":0.71,"unknown":0.5}}},
        "usage":{"input_tokens":500,"output_tokens":7}}"#;
    let (base, connections, rx) = spawn_mock(Reply::Response {
        status: 200,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let decision = client
        .choose(
            &plain_ctx(),
            "fix the parser",
            Path::new("/repo"),
            &candidates(),
        )
        .await
        .unwrap();
    let req = recorded(rx).await;

    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/systemone");
    assert_eq!(req.header("Authorization"), Some("Bearer test-key"));
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    assert_eq!(
        req.header("TypeSafe-Danger-Zone-Long-Context"),
        Some("true")
    );

    let body = req.json();
    assert_eq!(body["model"].as_str(), Some("jev-latest"));
    assert_eq!(body["state"]["task"].as_str(), Some("fix the parser"));
    assert_eq!(body["state"]["cwd"].as_str(), Some("/repo"));
    let q = &body["questions"]["model"];
    assert_eq!(q["type"].as_str(), Some("choice"));
    assert_eq!(
        q["instructions"].as_str(),
        Some(
            "Select the configured provider and model best suited to complete the task in state. Weigh the task's required reasoning depth, coding and tool needs, and the speed/cost implied by each candidate's description. Choose exactly one candidate."
        )
    );
    let criteria = q["criteria"].as_object().unwrap();
    assert_eq!(criteria.len(), 2);
    assert_eq!(
        criteria["c1"].as_str(),
        Some("provider alpha (api chat-completions); model m-fast; fast, cheap")
    );
    assert_eq!(
        criteria["c2"].as_str(),
        Some("provider bravo (api codex-app-server); model org/model")
    );

    assert_eq!(decision.provider, "bravo");
    assert_eq!(decision.model, "org/model");
    assert_eq!(decision.judge_model.as_deref(), Some("jev-1.14.0"));
    assert!((decision.confidence - 0.71).abs() < 1e-9);
    assert_eq!(decision.probabilities.len(), 2);
    assert!((decision.probabilities["alpha/m-fast"] - 0.29).abs() < 1e-9);
    assert!((decision.probabilities["bravo/org/model"] - 0.71).abs() < 1e-9);
    assert_eq!(decision.input_tokens, Some(500));
    assert_eq!(decision.output_tokens, Some(7));
}

#[tokio::test]
async fn choose_leaves_usage_absent() {
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"c1","confidence":1,"probabilities":{"c1":1}}}}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 200,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let decision = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &candidates())
        .await
        .unwrap();
    let _ = recorded(rx).await;
    assert_eq!(decision.input_tokens, None);
    assert_eq!(decision.output_tokens, None);
}

#[tokio::test]
async fn choose_rejects_no_candidates() {
    let (base, connections, _rx) = spawn_mock(Reply::Response {
        status: 200,
        body: "{}".into(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let err = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &[])
        .await
        .unwrap_err();
    assert_eq!(err.message, "systemone: no candidates to choose from");
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn choose_rejects_too_many_candidates() {
    let (base, connections, _rx) = spawn_mock(Reply::Response {
        status: 200,
        body: "{}".into(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let many: Vec<Candidate> = (0..256)
        .map(|i| Candidate {
            provider: "a".into(),
            model: format!("m{}", i),
            api: String::new(),
            description: String::new(),
        })
        .collect();
    let err = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &many)
        .await
        .unwrap_err();
    assert_eq!(
        err.message,
        "systemone: 256 candidates exceed the maximum of 255"
    );
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn choose_rejects_unknown_choice() {
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"nope","confidence":0.5,"probabilities":{}}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 200,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let err = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &candidates())
        .await
        .unwrap_err();
    let _ = recorded(rx).await;
    assert_eq!(
        err.message,
        "systemone: model \"jev-1.14.0\" chose unknown choice \"nope\""
    );
}

#[tokio::test]
async fn choose_rejects_non_choice_answer() {
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"noul","noul":0.5}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 200,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let err = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &candidates())
        .await
        .unwrap_err();
    let _ = recorded(rx).await;
    assert_eq!(
        err.message,
        "systemone: model \"jev-1.14.0\" answer type \"noul\" is not \"choice\""
    );
}

#[tokio::test]
async fn choose_clef_endpoint_and_result_envelope() {
    let response = r#"{"result":{"model":"clef","answers":{"model":{"type":"choice","choice":"c1"}},"usage":{"input_tokens":12}},"success":true}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 200,
        body: response.to_string(),
    })
    .await;
    let mut cfg = auto_config(&base);
    cfg.url = format!("{}/ai/run/@cf/cloudflare/clef", base);
    cfg.model = "clef".to_string();
    cfg.key = Some("key".to_string());
    let client = make_client(&cfg);
    let decision = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &[candidate_a()])
        .await
        .unwrap();
    let req = recorded(rx).await;
    assert_eq!(req.path, "/ai/run/@cf/cloudflare/clef");
    assert_eq!(req.header("Authorization"), Some("Bearer key"));
    assert_eq!(decision.model, "m");
    assert_eq!(decision.judge_model.as_deref(), Some("clef"));
    assert_eq!(decision.input_tokens, Some(12));
    assert_eq!(decision.output_tokens, None);
}

#[tokio::test]
async fn choose_reports_http_errors_with_redaction() {
    let response =
        r#"{"errors":[{"code":123,"message":"invalid model"},{"message":"test-key secret-task"}]}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 400,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let err = client
        .choose(
            &plain_ctx(),
            "secret-task",
            Path::new("/repo"),
            &[candidate_a()],
        )
        .await
        .unwrap_err();
    let _ = recorded(rx).await;
    assert!(err.message.contains("invalid model"), "{}", err.message);
    assert!(!err.message.contains("test-key"), "{}", err.message);
    assert!(!err.message.contains("secret-task"), "{}", err.message);
}

#[tokio::test]
async fn choose_reports_http_detail_without_leaking_key() {
    let response = r#"{"detail":"invalid API key"}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 401,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let err = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &[candidate_a()])
        .await
        .unwrap_err();
    let _ = recorded(rx).await;
    assert_eq!(err.message, "systemone: status 401: invalid API key");
    assert!(!err.message.contains("test-key"), "{}", err.message);
}

#[tokio::test]
async fn choose_reports_validation_detail() {
    let response = r#"{"detail":{"loc":["body","state"],"msg":"state is too long"}}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 422,
        body: response.to_string(),
    })
    .await;
    let client = make_client(&auto_config(&base));
    let task = "x".repeat(100_000);
    let err = client
        .choose(&plain_ctx(), &task, Path::new("/repo"), &[candidate_a()])
        .await
        .unwrap_err();
    let _ = recorded(rx).await;
    assert!(err.message.contains("422"), "{}", err.message);
    assert!(err.message.contains("state is too long"), "{}", err.message);
}

#[tokio::test]
async fn choose_handles_trailing_slash() {
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"c1","confidence":1,"probabilities":{"c1":1}}},"usage":{"input_tokens":1,"output_tokens":1}}"#;
    let (base, _, rx) = spawn_mock(Reply::Response {
        status: 200,
        body: response.to_string(),
    })
    .await;
    let cfg = auto_config(&format!("{}/prefix/", base));
    let client = make_client(&cfg);
    let _ = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &[candidate_a()])
        .await
        .unwrap();
    let req = recorded(rx).await;
    assert_eq!(req.path, "/prefix/v1/systemone");
}

#[tokio::test]
async fn choose_rejects_oversized_response() {
    let body = "x".repeat((1 << 20) + 1);
    let (base, _, rx) = spawn_mock(Reply::Response { status: 200, body }).await;
    let client = make_client(&auto_config(&base));
    let err = client
        .choose(&plain_ctx(), "task", Path::new("/repo"), &[candidate_a()])
        .await
        .unwrap_err();
    let _ = recorded(rx).await;
    assert_eq!(
        err.message,
        "systemone: response body too large (limit 1048576 bytes)"
    );
}

#[tokio::test]
async fn choose_times_out() {
    let (base, _, _rx) = spawn_mock(Reply::Hang).await;
    let mut cfg = auto_config(&base);
    cfg.timeout_seconds = 1;
    let client = make_client(&cfg);
    // Hard bound so a broken timeout can never hang the test.
    let call = tokio::time::timeout(
        Duration::from_secs(5),
        client.choose(&plain_ctx(), "task", Path::new("/repo"), &[candidate_a()]),
    )
    .await
    .expect("choose did not return within 5s");
    let err = call.expect_err("expected a timeout error");
    assert!(
        err.message.starts_with("systemone: request:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn choose_honors_run_cancellation() {
    let (base, _, _rx) = spawn_mock(Reply::Hang).await;
    let client = make_client(&auto_config(&base));
    let ctx = RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    };
    let token = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    let err = client
        .choose(&ctx, "task", Path::new("/repo"), &[candidate_a()])
        .await
        .unwrap_err();
    assert_eq!(err.message, "systemone: request: context canceled");
}

// ---------------------------------------------------------------------------
// Explicit `null` in response fields (review finding I1)
//
// Go `json.Unmarshal` treats `null` for a non-pointer struct field as "no
// effect" (the zero value), not a type error: map -> nil, string -> "", float
// -> 0. `#[serde(default)]` only handles a *missing* field, so these tests
// pin the required acceptance of explicit `null`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn choose_accepts_null_answers_as_no_answer() {
    // Go: answer.Answers is nil -> "returned no answer for question".
    let response =
        r#"{"model":"jev-1.14.0","answers":null,"usage":{"input_tokens":10,"output_tokens":1}}"#;
    let err = choose_against(response)
        .await
        .expect_err("expected a no-answer error");
    assert_eq!(
        err,
        "systemone: model \"jev-1.14.0\" returned no answer for question \"model\""
    );
}

#[tokio::test]
async fn choose_accepts_null_type_as_non_choice() {
    // Go: choice.Type is "" -> "answer type \"\" is not \"choice\"".
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":null,"choice":"c1"}}}"#;
    let err = choose_against(response)
        .await
        .expect_err("expected a non-choice error");
    assert_eq!(
        err,
        "systemone: model \"jev-1.14.0\" answer type \"\" is not \"choice\""
    );
}

#[tokio::test]
async fn choose_accepts_null_choice_as_unknown() {
    // Go: choice.Choice is "" -> "chose unknown choice \"\"".
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":null}}}"#;
    let err = choose_against(response)
        .await
        .expect_err("expected an unknown-choice error");
    assert_eq!(
        err,
        "systemone: model \"jev-1.14.0\" chose unknown choice \"\""
    );
}

#[tokio::test]
async fn choose_accepts_null_confidence_as_zero() {
    // Go: choice.Confidence is 0 -> valid selection with confidence 0.
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"c1","confidence":null,"probabilities":{}}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
    let decision = choose_against(response)
        .await
        .expect("expected a valid selection");
    assert!(decision.confidence.abs() < 1e-9, "{}", decision.confidence);
    assert_eq!(decision.provider, "alpha");
}

#[tokio::test]
async fn choose_accepts_null_probabilities_as_valid_selection() {
    // Go: `if choice.Probabilities != nil` guard skips nil -> empty result map.
    let response = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"c1","confidence":0.9,"probabilities":null}},"usage":{"input_tokens":12,"output_tokens":2}}"#;
    let decision = choose_against(response)
        .await
        .expect("expected a valid selection");
    assert_eq!(decision.provider, "alpha");
    assert_eq!(decision.model, "m-fast");
    assert_eq!(decision.probabilities.len(), 0);
    assert_eq!(decision.input_tokens, Some(12));
    assert_eq!(decision.output_tokens, Some(2));
}

#[tokio::test]
async fn choose_accepts_null_model_as_empty() {
    // Go: answer.Model is "" -> no-answer error quoting the empty model.
    let response = r#"{"model":null,"answers":null}"#;
    let err = choose_against(response)
        .await
        .expect_err("expected a no-answer error");
    assert_eq!(
        err,
        "systemone: model \"\" returned no answer for question \"model\""
    );
}
