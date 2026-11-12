//! Task 9: shared provider transport, debug SSE capture, upstream recorder,
//! SSE byte/line reader, factory registry, and the JSON replay conventions.
//!
//! Ports the observable contracts from Go `internal/provider/transport.go`,
//! `transport_debug_test.go`, `transport_timeout_test.go`, `factory.go`, and the
//! design's parse-equivalence / opaque-payload rules. The exact subagent-mcp
//! authored strings (unsupported-api error, debug file format) are asserted;
//! reqwest/hyper transport text is only matched semantically.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Map, Value};
use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::provider::factory;
use subagent_mcp::provider::sse;
use subagent_mcp::provider::transport;
use subagent_mcp::provider::transport::{
    DEBUG_STREAM_ENV, DRAIN_LIMIT, DRAIN_TIMEOUT, PooledClient, UPSTREAM_SCAN_LIMIT,
};
use subagent_mcp::provider::{
    DeltaSink, Provider, ProviderError, TransportKind, TurnRequest, TurnResult, Usage,
};
use subagent_mcp::util::run::RunContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Local HTTP mock (stand-in for Go's httptest.Server)
// ---------------------------------------------------------------------------

enum Reply {
    /// Accept, read the request, then never send response headers.
    Hang,
    /// Write the raw response bytes, then optionally keep the socket open.
    Respond {
        raw: Vec<u8>,
        tail: Option<Duration>,
    },
}

async fn spawn_mock(reply: Reply) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = socket.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        match reply {
            Reply::Hang => {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            Reply::Respond { raw, tail } => {
                let _ = socket.write_all(&raw).await;
                if let Some(t) = tail {
                    tokio::time::sleep(t).await;
                }
            }
        }
    });
    base
}

fn http_response(content_type: &str, body: &[u8], keep_open: bool) -> Vec<u8> {
    let connection = if keep_open { "keep-alive" } else { "close" };
    let mut raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    raw
}

fn request_for(base: &str, path: &str) -> reqwest::Request {
    let url = Url::parse(&format!("{base}{path}")).unwrap();
    reqwest::Request::new(Method::GET, url)
}

// ---------------------------------------------------------------------------
// Factory registry seam
// ---------------------------------------------------------------------------

struct NopProvider {
    name: String,
}

#[async_trait::async_trait]
impl Provider for NopProvider {
    fn name(&self) -> &str {
        &self.name
    }
    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        Ok(TurnResult::default())
    }
}

fn provider_config(api: Api) -> ProviderConfig {
    ProviderConfig {
        api,
        base_url: "https://example.invalid/v1".to_string(),
        env_key: None,
        key: None,
        command: None,
        default_model: "m".to_string(),
        models: Vec::new(),
        effort_map: BTreeMap::new(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

#[test]
fn factory_registers_and_rejects_unsupported_api() {
    factory::register(
        Api::ChatCompletions.as_str(),
        Arc::new(
            |name: &str,
             _cfg: &ProviderConfig,
             _key: &str|
             -> Result<Arc<dyn Provider>, ProviderError> {
                Ok(Arc::new(NopProvider {
                    name: name.to_string(),
                }))
            },
        ),
    );

    // A registered api is found and its constructor is invoked.
    let provider = factory::new("mine", &provider_config(Api::ChatCompletions), "k").unwrap();
    assert_eq!(provider.name(), "mine");

    // An unregistered api produces the exact subagent-mcp-authored error.
    let err = factory::new("x", &provider_config(Api::Messages), "k")
        .err()
        .expect("an unregistered api must fail");
    assert_eq!(
        err.message,
        r#"unsupported api "messages" (is its adapter package imported?)"#
    );
    assert_eq!(DEBUG_STREAM_ENV, "SUBAGENT_MCP_DEBUG_STREAM");
}

// ---------------------------------------------------------------------------
// extra_body merge (extra keys win)
// ---------------------------------------------------------------------------

#[test]
fn extra_body_merges_with_extra_keys_winning() {
    let original = br#"{"model":"m","stream":true,"max_tokens":100}"#;
    let mut extra = Map::new();
    extra.insert("stream".into(), serde_json::json!(false));
    extra.insert("custom".into(), serde_json::json!({"k":"v"}));

    let merged = transport::merge_extra_body(original, &extra).unwrap();
    let v: Value = serde_json::from_slice(&merged).unwrap();
    assert_eq!(v["model"], serde_json::json!("m"), "original preserved");
    assert_eq!(v["stream"], serde_json::json!(false), "extra wins");
    assert_eq!(v["custom"], serde_json::json!({"k":"v"}), "extra added");
    assert_eq!(
        v["max_tokens"],
        serde_json::json!(100),
        "original preserved"
    );

    // A non-object JSON body is returned unchanged, like Go's mergeJSONBody.
    let array = br#"[1,2,3]"#;
    assert_eq!(
        transport::merge_extra_body(array, &extra).unwrap(),
        array.to_vec()
    );
    // A malformed body is returned unchanged (parse only, never fatal).
    let broken = br#"not json"#;
    assert_eq!(
        transport::merge_extra_body(broken, &extra).unwrap(),
        broken.to_vec()
    );
}

// ---------------------------------------------------------------------------
// Debug SSE capture: exact file name, 0600, header/body format, no query string
// ---------------------------------------------------------------------------

const STREAM_BODY: &str =
    "data: {\"choices\":[{\"delta\":{\"reasoning\":\"hidden\"}}]}\n\ndata: [DONE]\n\n";

#[test]
fn debug_stream_capture_uses_exact_filename_mode_and_format() {
    // Use a fresh subdirectory (Go's `t.TempDir()/streams`), so the capture
    // actually creates it with 0700 rather than reusing an existing tempdir.
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("streams");
    let url = Url::parse("http://127.0.0.1:8080/chat/completions?key=secret-in-query").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("x-request-id", HeaderValue::from_static("req-123"));
    headers.insert("set-cookie", HeaderValue::from_static("session=secret"));

    let mut capture = transport::DebugCapture::open(
        &dir,
        TransportKind::ChatCompletions,
        &Method::POST,
        &url,
        StatusCode::OK,
        &headers,
    )
    .unwrap();
    capture.write_chunk(STREAM_BODY.as_bytes());
    let path = capture.path().to_path_buf();
    drop(capture);

    // `<UTC>-<kind>-<n>.sse`
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let pattern = regex::Regex::new(r"^\d{8}T\d{6}\.\d{3}-chat-completions-\d+\.sse$").unwrap();
    assert!(
        pattern.is_match(&name),
        "unexpected debug file name: {name}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "debug file must be 0600");
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "debug directory must be 0700");
    }

    let text = std::fs::read_to_string(&path).unwrap();
    for want in [
        "# POST http://127.0.0.1:8080/chat/completions\n",
        "# status 200\n",
        "# Content-Type: text/event-stream\n",
        "# X-Request-Id: req-123\n",
        STREAM_BODY,
    ] {
        assert!(text.contains(want), "recording lacks {want:?}:\n{text}");
    }
    for leak in ["secret-in-query", "session=secret", "key=secret-in-query"] {
        assert!(!text.contains(leak), "recording leaks {leak:?}:\n{text}");
    }
    assert_eq!(transport::kind_label(TransportKind::Responses), "responses");
    assert_eq!(transport::kind_label(TransportKind::Messages), "messages");
}

// ---------------------------------------------------------------------------
// Upstream recorder scans only the first 16 KiB for `"provider":"..."`
// ---------------------------------------------------------------------------

#[test]
fn upstream_recorder_finds_provider_in_first_16kib() {
    let mut rec = transport::UpstreamRecorder::new();
    rec.feed(b"data: {\"id\":\"x\",\"provider\":");
    rec.feed(b"\"openai/gpt-4\",\"model\":\"m\"}\n\n");
    assert_eq!(rec.name(), Some("openai/gpt-4"));

    // A field beyond the first 16 KiB is never seen.
    let mut late = transport::UpstreamRecorder::new();
    late.feed(&vec![b' '; UPSTREAM_SCAN_LIMIT]);
    late.feed(br#"{"provider":"too-late"}"#);
    assert_eq!(late.name(), None);

    // No field at all, and Default is empty.
    let mut absent = transport::UpstreamRecorder::new();
    absent.feed(br#"data: {"choices":[]}"#);
    assert_eq!(absent.name(), None);
    assert_eq!(transport::UpstreamRecorder::default().name(), None);
}

// ---------------------------------------------------------------------------
// Opaque payloads preserved as raw JSON bytes; parse-only validation
// ---------------------------------------------------------------------------

#[test]
fn opaque_payloads_replay_original_bytes() {
    let original = r#"{"a":1e0,"b":"\u0061","c":"<script>","nested":{"x":1}}"#;
    let raw = transport::preserve_opaque(original.as_bytes()).unwrap();
    assert_eq!(
        raw.get(),
        original,
        "Box<RawValue> must replay original bytes"
    );
    assert_eq!(
        transport::opaque_len(Some(raw.as_ref())),
        original.len(),
        "raw byte length counts toward history"
    );
    assert_eq!(transport::opaque_len(None), 0);
    assert!(
        transport::preserve_opaque(b"{ not json").is_err(),
        "parse must validate the payload"
    );
}

// ---------------------------------------------------------------------------
// Ordinary JSON compares parse-equivalent, not byte-identical
// ---------------------------------------------------------------------------

#[test]
fn ordinary_json_compares_parse_equivalent() {
    // `\u0061`, `1e0`, and HTML-escapable `\u003c...` all re-encode differently.
    let original = r#"{"esc":"\u0061","num":1e0,"html":"\u003cscript\u003e"}"#;
    let reencoded =
        serde_json::to_string(&serde_json::from_str::<Value>(original).unwrap()).unwrap();
    assert_ne!(original, reencoded, "re-encoding must change the bytes");
    assert!(
        transport::json_parse_equivalent(original, &reencoded),
        "re-encoded value must be parse-equivalent"
    );
    assert!(
        transport::json_parse_equivalent(original, r#"{"esc":"a","num":1,"html":"<script>"}"#),
        "1e0 == 1 and escapes decode to the same value"
    );
    assert!(
        !transport::json_parse_equivalent(original, r#"{"esc":"b","num":1,"html":"<script>"}"#),
        "a genuinely different value must not compare equal"
    );
}

// ---------------------------------------------------------------------------
// Usage: absent stays None, explicit all-zero stays Some
// ---------------------------------------------------------------------------

#[test]
fn absent_usage_is_none_explicit_zero_usage_is_some() {
    assert_eq!(transport::usage_from_json(None), None);
    let zero = serde_json::json!({"input":0,"cached":0,"output":0,"reasoning":0,"total":0});
    assert_eq!(
        transport::usage_from_json(Some(&zero)),
        Some(Usage::default())
    );
    assert_eq!(
        transport::usage_from_json(Some(&serde_json::json!({}))),
        Some(Usage::default())
    );
    let real = serde_json::json!({"input":10,"cached":2,"output":3,"reasoning":1,"total":13});
    assert_eq!(
        transport::usage_from_json(Some(&real)),
        Some(Usage {
            input: 10,
            cached: 2,
            output: 3,
            reasoning: 1,
            total: 13,
        })
    );
}

// ---------------------------------------------------------------------------
// Shared client / header-only timeout / PooledClient
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shared_client_executes_a_request() {
    let raw = http_response("text/plain", b"ok", false);
    let base = spawn_mock(Reply::Respond { raw, tail: None }).await;
    let client = transport::shared_client(TransportKind::Messages, &base);
    let resp = client
        .get(format!("{base}/v1/messages"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn header_timeout_slow_header_fails() {
    let base = spawn_mock(Reply::Hang).await;
    let client = transport::shared_client(TransportKind::Responses, &base);
    let req = request_for(&base, "/responses");
    let err = transport::execute_with_header_timeout(&client, req, Duration::from_millis(80))
        .await
        .expect_err("a server that never answers must fail the header timeout");
    assert!(
        err.message.contains("timeout"),
        "unexpected error: {}",
        err.message
    );
}

#[tokio::test]
async fn header_timeout_long_stream_after_headers_succeeds() {
    let raw = http_response("text/event-stream", STREAM_BODY.as_bytes(), false);
    let base = spawn_mock(Reply::Respond { raw, tail: None }).await;
    let client = transport::shared_client(TransportKind::ChatCompletions, &base);
    let req = request_for(&base, "/chat/completions");
    let resp = transport::execute_with_header_timeout(&client, req, Duration::from_secs(600))
        .await
        .unwrap();

    // Jump past the 10-minute header timeout with paused time. The body must
    // still be fully readable: a total-request timeout would have cut it off.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    let got = resp.text().await.unwrap();
    assert_eq!(got, STREAM_BODY);
    assert_eq!(
        transport::DEFAULT_RESPONSE_HEADER_TIMEOUT,
        Duration::from_secs(600)
    );
}

#[tokio::test]
async fn pooled_client_executes_and_reads_the_body() {
    let raw = http_response("text/event-stream", b"data: hello\n\n", false);
    let base = spawn_mock(Reply::Respond { raw, tail: None }).await;
    let client = PooledClient::new(TransportKind::ChatCompletions, &base);
    let req = request_for(&base, "/chat/completions");
    let mut resp = client.execute(req, Duration::from_secs(5)).await.unwrap();
    assert_eq!(resp.upstream(), None);
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.unwrap() {
        out.extend_from_slice(&chunk);
    }
    assert_eq!(out, b"data: hello\n\n");
    resp.close().await;
}

// ---------------------------------------------------------------------------
// Drop-drain is bounded by 64 KiB and 250 ms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dropped_response_drain_is_bounded() {
    // A 1 MiB declared body, but only 8 KiB is sent; the server then stalls and
    // keeps the socket open. A drain that waited for EOF would hang for 30 s.
    let mut raw = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1048576\r\nConnection: keep-alive\r\n\r\n",
    )
    .into_bytes();
    raw.extend(std::iter::repeat_n(b'x', 8 * 1024));
    let base = spawn_mock(Reply::Respond {
        raw,
        tail: Some(Duration::from_secs(30)),
    })
    .await;
    let client = transport::shared_client(TransportKind::ChatCompletions, &base);
    let req = request_for(&base, "/stream");
    let resp = transport::execute_with_header_timeout(&client, req, Duration::from_secs(5))
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let drained = transport::drain_response(resp).await;
    let elapsed = start.elapsed();
    assert!(
        drained <= DRAIN_LIMIT,
        "drain exceeded 64 KiB: {drained} bytes"
    );
    assert!(
        elapsed <= DRAIN_TIMEOUT + Duration::from_millis(200),
        "drain exceeded the 250 ms bound: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// SSE LineReader: arbitrary chunk boundaries from an external crate
// ---------------------------------------------------------------------------

struct Chunked {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
}

impl Read for Chunked {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[test]
fn line_reader_preserves_arbitrary_chunk_boundaries() {
    let data = b"data: one\n\ndata: two\r\n\n: ping\n\ndata: three";
    let expected: Vec<Vec<u8>> = vec![
        b"data: one".to_vec(),
        b"".to_vec(),
        b"data: two".to_vec(),
        b"".to_vec(),
        b": ping".to_vec(),
        b"".to_vec(),
        b"data: three".to_vec(),
    ];
    for chunk in [1usize, 2, 3, 5, 7, 100] {
        let reader = Chunked {
            data: data.to_vec(),
            pos: 0,
            chunk,
        };
        let mut lr = sse::LineReader::new(reader, 1 << 20);
        let mut lines = Vec::new();
        while let Some(line) = lr.read_line().unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, expected, "chunk size {chunk}");
    }
}
