//! Shared provider transport.
//!
//! Ports the behavior of Go `internal/provider/transport.go`: one pooled
//! `reqwest::Client` per `(kind, base URL)`, a header-only response timeout,
//! `extra_body` merging, opt-in debug SSE capture, an upstream recorder, and a
//! bounded drain-on-drop so streamed connections are reused. Provider framing
//! state machines live in the adapter modules; this file owns transport only.
//!
//! The `reqwest` client is built with the Task 1 dependency configuration
//! (`default-features = false`, `rustls-tls-manual-roots`, rustls `ring`) and
//! [`crate::provider::tls::client_config`]. HTTP/2 is negotiated via the ALPN
//! protocols set on that TLS config, and proxy-from-environment is reqwest's
//! default (`auto_sys_proxy` reads `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/
//! `NO_PROXY`, matching Go's `http.ProxyFromEnvironment`). Neither is disabled
//! here, and no `Client::timeout` is set: only [`execute_with_header_timeout`]
//! bounds the wait for response headers, so long SSE bodies are never cut off.
//!
//! [`shared_client`] is the **internal pool only**. It returns a bare
//! `reqwest::Client` with no response-header bound and no `extra_body`; a
//! direct `.get().send()` through it is NOT the adapter request path. Adapters
//! must send through [`PooledClient`], whose [`PooledClient::execute`] and
//! [`PooledClient::execute_json`] methods enforce the shared 10-minute
//! [`DEFAULT_RESPONSE_HEADER_TIMEOUT`] (a header-only timer, never a total
//! timeout) and apply the configured `extra_body` merge automatically.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::value::RawValue;

use crate::config::ProviderConfig;
use crate::provider::{ProviderError, TransportKind, Usage};

// --- Constants (Go transport.go) ------------------------------------------

/// Bounds only the wait for response headers, never the response body.
pub const DEFAULT_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Maximum bytes drained from an unread body when a response is dropped.
pub const DRAIN_LIMIT: usize = 64 << 10;
/// Maximum time spent draining an unread body.
pub const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
/// How much of the stream is scanned for the upstream `"provider"` field.
pub const UPSTREAM_SCAN_LIMIT: usize = 16 << 10;
/// Env var naming the directory that receives raw debug SSE captures.
pub const DEBUG_STREAM_ENV: &str = "SUBAGENT_MCP_DEBUG_STREAM";

/// Wire label for a transport kind; used as the registry key and the `<kind>`
/// segment of a debug capture file name.
pub fn kind_label(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::ChatCompletions => "chat-completions",
        TransportKind::Responses => "responses",
        TransportKind::Messages => "messages",
    }
}

// --- Shared client registry ------------------------------------------------

static CLIENTS: LazyLock<Mutex<HashMap<String, reqwest::Client>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Return the connection pool shared by every adapter built for the same
/// `(kind, base_url)`. `reqwest::Client` clones are cheap (an internal `Arc`),
/// so every caller reuses one TLS/TCP pool without rebuilding the client.
///
/// This is the **pool**, not the adapter send path. It intentionally carries no
/// `Client::timeout` (which would truncate long SSE bodies) and no
/// `extra_body`; a direct `.get().send()` here has no response-header bound.
/// Adapter requests must go through [`PooledClient::execute`] /
/// [`PooledClient::execute_json`], which enforce
/// [`DEFAULT_RESPONSE_HEADER_TIMEOUT`] and merge `extra_body`.
pub fn shared_client(
    kind: TransportKind,
    base_url: &str,
) -> Result<reqwest::Client, ProviderError> {
    let key = format!("{}\x00{}", kind_label(kind), base_url);
    let mut clients = CLIENTS.lock().unwrap();
    if let Some(client) = clients.get(&key) {
        return Ok(client.clone());
    }
    let client = build_client()?;
    clients.insert(key, client.clone());
    Ok(client)
}

fn build_client() -> Result<reqwest::Client, ProviderError> {
    let tls = crate::provider::tls::client_config().map_err(|e| ProviderError {
        message: format!("provider transport: {e}"),
    })?;
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        // Match Go's http.DefaultTransport dial (30 s) and idle (90 s) bounds.
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| ProviderError {
            message: format!("provider transport: build reqwest client: {e}"),
        })
}

/// Execute `req` with a timer around **only** `client.execute()`. reqwest
/// resolves `execute()` as soon as response headers arrive (the body streams
/// lazily afterwards), so this bounds the header wait without imposing a
/// total-request timeout that would truncate a long SSE stream.
pub async fn execute_with_header_timeout(
    client: &reqwest::Client,
    req: reqwest::Request,
    timeout: Duration,
) -> Result<reqwest::Response, ProviderError> {
    match tokio::time::timeout(timeout, client.execute(req)).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => Err(ProviderError {
            message: e.to_string(),
        }),
        Err(_) => Err(ProviderError {
            message: format!("response header timeout after {}ms", timeout.as_millis()),
        }),
    }
}

// --- extra_body merge (Go mergeJSONBody) -----------------------------------

/// Merge `extra` into a JSON object request body, with extra keys winning. A
/// body that is not a JSON object, or is not valid JSON, is returned unchanged.
/// The request body is re-serialized; opaque *replay* payloads are a separate
/// concern (see [`preserve_opaque`]).
pub fn merge_extra_body(
    original: &[u8],
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<u8>, ProviderError> {
    if extra.is_empty() {
        return Ok(original.to_vec());
    }
    let mut value: serde_json::Value = match serde_json::from_slice(original) {
        Ok(v) => v,
        Err(_) => return Ok(original.to_vec()),
    };
    let Some(obj) = value.as_object_mut() else {
        return Ok(original.to_vec());
    };
    for (key, val) in extra {
        obj.insert(key.clone(), val.clone());
    }
    serde_json::to_vec(&value).map_err(|e| ProviderError {
        message: format!("extra_body merge: {e}"),
    })
}

// --- Debug SSE capture (Go recordStream) -----------------------------------

static DEBUG_SEQ: AtomicU64 = AtomicU64::new(0);

/// A raw response capture: `<dir>/<UTC>-<kind>-<n>.sse`, mode 0600, with a
/// small header block (method, URL without query/credentials, status, and the
/// three allowlisted headers) followed by the body exactly as received. Request
/// headers, request bodies, query strings, and cookies are never written.
pub struct DebugCapture {
    file: std::fs::File,
    path: PathBuf,
}

impl DebugCapture {
    /// Open a capture file and write its header block. A sequence counter keeps
    /// names unique; `O_EXCL` (`create_new`) avoids overwriting a prior capture.
    pub fn open(
        dir: &Path,
        kind: TransportKind,
        method: &reqwest::Method,
        url: &reqwest::Url,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<Self, std::io::Error> {
        create_debug_dir(dir)?;
        let utc = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f");
        let seq = DEBUG_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let label = kind_label(kind);
        let name = format!("{utc}-{label}-{seq}.sse");
        let path = dir.join(name);
        let mut file = open_debug_file(&path)?;

        // Reconstruct scheme://host[:port]<path> by hand: no query string, no
        // userinfo/credentials, no request headers or body.
        let host = url.host_str().unwrap_or("");
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        let mut header = format!(
            "# {} {}://{}{}\n# status {}\n",
            method,
            url.scheme(),
            authority,
            url.path(),
            status.as_u16()
        );
        for name in ["Content-Type", "X-Request-Id", "Request-Id"] {
            if let Some(value) = headers.get(name)
                && let Ok(text) = value.to_str()
                && !text.is_empty()
            {
                header.push_str(&format!("# {name}: {text}\n"));
            }
        }
        header.push('\n');
        use std::io::Write;
        file.write_all(header.as_bytes())?;
        Ok(Self { file, path })
    }

    /// Append raw response body bytes exactly as received.
    pub fn write_chunk(&mut self, bytes: &[u8]) {
        use std::io::Write;
        // A capture write failure must never break the response (Go logs once
        // and passes the body through untouched).
        let _ = self.file.write_all(bytes);
    }

    /// Path of the capture file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn create_debug_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)
}

#[cfg(not(unix))]
fn create_debug_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_debug_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    opts.open(path)
}

#[cfg(not(unix))]
fn open_debug_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

// --- Upstream recorder (Go upstreamBody) -----------------------------------

static UPSTREAM_FIELD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#""provider"\s*:\s*"([^"\\]{1,64})""#).expect("valid upstream-field regex")
});

/// Scans the first [`UPSTREAM_SCAN_LIMIT`] bytes of a response for the
/// `"provider":"..."` field a gateway reports in its first stream chunk.
pub struct UpstreamRecorder {
    window: Vec<u8>,
    name: Option<String>,
    done: bool,
}

impl UpstreamRecorder {
    pub fn new() -> Self {
        Self {
            window: Vec::new(),
            name: None,
            done: false,
        }
    }

    /// Feed the next body chunk. Once the field is found, or the scan window is
    /// exhausted, recording stops (matching Go's bounded scan).
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.done {
            return;
        }
        self.window.extend_from_slice(chunk);
        let text = String::from_utf8_lossy(&self.window);
        if let Some(caps) = UPSTREAM_FIELD.captures(&text) {
            self.name = Some(caps[1].to_string());
            self.done = true;
        } else if self.window.len() >= UPSTREAM_SCAN_LIMIT {
            self.done = true;
        }
    }

    /// The recorded upstream name, or `None` when the API reports none.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl Default for UpstreamRecorder {
    fn default() -> Self {
        Self::new()
    }
}

// --- Bounded drain + PooledClient ------------------------------------------

/// Prefix of `chunk` that still fits in the remaining drain budget. Clamping
/// here is what makes the cap exact even when a single chunk is larger than the
/// whole budget (Go `io.CopyN` reads exactly 64 KiB).
fn cap_chunk(chunk: &[u8], total: usize) -> &[u8] {
    let take = (DRAIN_LIMIT - total).min(chunk.len());
    &chunk[..take]
}

/// Drain an unread response body up to **exactly** [`DRAIN_LIMIT`] bytes and
/// [`DRAIN_TIMEOUT`], teeing only that capped prefix to the debug capture and
/// upstream recorder. Returns the number of bytes drained. This is the core of
/// the drain-on-drop behavior: a sentinel-but-still-open stream cannot stall
/// the caller, and Go's `io.CopyN` reads exactly 64 KiB (the last chunk is
/// clamped, never added whole).
async fn bounded_drain(
    resp: reqwest::Response,
    mut debug: Option<DebugCapture>,
    upstream: Arc<Mutex<UpstreamRecorder>>,
) -> usize {
    let mut resp = resp;
    let mut total = 0usize;
    {
        let drain = async {
            while total < DRAIN_LIMIT {
                match resp.chunk().await {
                    Ok(Some(bytes)) => {
                        // Retain at most the remaining budget: clamp an
                        // oversized chunk instead of adding it whole.
                        let capped = cap_chunk(&bytes, total);
                        total += capped.len();
                        if let Some(d) = debug.as_mut() {
                            d.write_chunk(capped);
                        }
                        upstream.lock().unwrap().feed(capped);
                        // The `while` guard stops immediately once total
                        // reaches DRAIN_LIMIT; no further chunk is read.
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        };
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
    }
    total
}

/// Drain a bare `reqwest::Response` with the standard bounds and explicit
/// recorders. Exposed so the exact-cap and capped-prefix contracts are directly
/// testable; returns exactly the number of bytes retained (≤ [`DRAIN_LIMIT`]).
pub async fn drain_response_with(
    resp: reqwest::Response,
    debug: Option<DebugCapture>,
    upstream: Arc<Mutex<UpstreamRecorder>>,
) -> usize {
    bounded_drain(resp, debug, upstream).await
}

/// Drain a bare `reqwest::Response` with the standard bounds (no recorders).
pub async fn drain_response(resp: reqwest::Response) -> usize {
    drain_response_with(resp, None, Arc::new(Mutex::new(UpstreamRecorder::new()))).await
}

/// A response whose unread body is drained on drop, so the pooled connection
/// can be reused like Go's transport. Reading through [`PooledResponse::chunk`]
/// tees bytes to the debug capture and upstream recorder.
pub struct PooledResponse {
    inner: Option<reqwest::Response>,
    debug: Option<DebugCapture>,
    upstream: Arc<Mutex<UpstreamRecorder>>,
}

impl PooledResponse {
    /// Read the next body chunk, teeing it to the recorders.
    pub async fn chunk(&mut self) -> Result<Option<bytes::Bytes>, ProviderError> {
        let Some(resp) = self.inner.as_mut() else {
            return Ok(None);
        };
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                if let Some(d) = self.debug.as_mut() {
                    d.write_chunk(&bytes);
                }
                self.upstream.lock().unwrap().feed(&bytes);
                Ok(Some(bytes))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(ProviderError {
                message: e.to_string(),
            }),
        }
    }

    /// The upstream name recorded so far (the adapter copies this into
    /// `TurnResult::upstream` at the end of the turn).
    pub fn upstream(&self) -> Option<String> {
        self.upstream.lock().unwrap().name().map(str::to_string)
    }

    /// HTTP status of the held response. Adapters need this to tell a
    /// retryable 429/5xx stream-setup failure from a permanent 4xx one (Go's
    /// `isFailureStatusCode` + `APIError.HTTPStatusCode`). Read-only: the
    /// response body still streams lazily through [`PooledResponse::chunk`].
    pub fn status(&self) -> Option<reqwest::StatusCode> {
        self.inner.as_ref().map(|r| r.status())
    }

    /// Response headers of the held response. Adapters need `Retry-After` /
    /// `x-should-retry` to apply the SDK retry policy. Read-only: the body
    /// still streams lazily through [`PooledResponse::chunk`].
    pub fn headers(&self) -> Option<&reqwest::header::HeaderMap> {
        self.inner.as_ref().map(|r| r.headers())
    }

    /// Explicitly finish the response with the bounded drain. Taking `inner`
    /// prevents the `Drop` impl from draining a second time.
    pub async fn close(mut self) {
        if let Some(resp) = self.inner.take() {
            let debug = self.debug.take();
            let upstream = Arc::clone(&self.upstream);
            bounded_drain(resp, debug, upstream).await;
        }
    }
}

impl Drop for PooledResponse {
    fn drop(&mut self) {
        let Some(resp) = self.inner.take() else {
            return;
        };
        let debug = self.debug.take();
        let upstream = Arc::clone(&self.upstream);
        // Dropping the response mid-stream: spawn the bounded drain so the
        // connection is reusable. Best-effort — without a live runtime, simply
        // dropping `resp` closes the connection.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            drop(handle.spawn(async move {
                bounded_drain(resp, debug, upstream).await;
            }));
        }
    }
}

/// Merge `extra` into a POST request carrying a byte-backed JSON body (Go's
/// `mergeJSONBody` inside `drainingTransport.RoundTrip`). Only POSTs whose
/// `Content-Type` contains `json` are merged; anything else is left untouched.
/// Applied by the single send path so no adapter request can bypass it.
fn apply_extra_body(
    req: &mut reqwest::Request,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), ProviderError> {
    if *req.method() != reqwest::Method::POST {
        return Ok(());
    }
    let is_json = req
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    if !is_json {
        return Ok(());
    }
    let Some(original) = req.body().and_then(|b| b.as_bytes()) else {
        // A streaming body cannot be merged synchronously; adapters build
        // JSON bodies with `execute_json`, which is always byte-backed.
        return Ok(());
    };
    let merged = merge_extra_body(original, extra)?;
    *req.body_mut() = Some(reqwest::Body::from(merged));
    Ok(())
}

/// A client over the shared pool that wraps responses with debug capture,
/// upstream recording, and bounded drain-on-drop. This is the **adapter send
/// path**: it enforces [`DEFAULT_RESPONSE_HEADER_TIMEOUT`] and applies the
/// configured `extra_body` merge to every POST JSON body automatically (Go
/// `PooledClient` / `PooledClientWithBody`). Adapters must not send through
/// [`shared_client`] directly.
pub struct PooledClient {
    http: reqwest::Client,
    kind: TransportKind,
    extra_body: Option<serde_json::Map<String, serde_json::Value>>,
}

impl PooledClient {
    /// Build a client with no `extra_body` (Go `PooledClient`).
    pub fn new(kind: TransportKind, base_url: &str) -> Result<Self, ProviderError> {
        Self::with_extra_body(kind, base_url, None)
    }

    /// Build a client carrying `extra_body`, merged into every POST JSON
    /// request body automatically (Go `PooledClientWithBody`).
    pub fn with_extra_body(
        kind: TransportKind,
        base_url: &str,
        extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            http: shared_client(kind, base_url)?,
            kind,
            extra_body,
        })
    }

    /// Build straight from a resolved [`ProviderConfig`]: base URL and
    /// `extra_body` come from config, so adapters cannot forget the merge.
    pub fn from_config(kind: TransportKind, cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        Self::with_extra_body(kind, &cfg.base_url, cfg.extra_body.clone())
    }

    /// Primary adapter entry: build a JSON POST from parts and send it. The
    /// `extra_body` merge and the enforced 10-minute header timeout are applied
    /// automatically through [`Self::execute`].
    pub async fn execute_json(
        &self,
        url: reqwest::Url,
        headers: reqwest::header::HeaderMap,
        body: &[u8],
    ) -> Result<PooledResponse, ProviderError> {
        let mut req = reqwest::Request::new(reqwest::Method::POST, url);
        *req.headers_mut() = headers;
        req.headers_mut().insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        *req.body_mut() = Some(reqwest::Body::from(body.to_vec()));
        self.execute(req).await
    }

    /// Send `req` with the enforced 10-minute response-header timeout and the
    /// automatic `extra_body` merge. Every execute variant funnels through
    /// here, so an adapter cannot bypass the merge or the header bound.
    pub async fn execute(&self, req: reqwest::Request) -> Result<PooledResponse, ProviderError> {
        self.send(req, DEFAULT_RESPONSE_HEADER_TIMEOUT).await
    }

    /// Like [`Self::execute`] but with an explicit header timeout. Kept for
    /// tests and special cases; the normal adapter path is [`Self::execute`],
    /// which enforces the shared 10-minute default just like Go's fixed
    /// `ResponseHeaderTimeout` on the pooled transport.
    pub async fn execute_with_timeout(
        &self,
        req: reqwest::Request,
        timeout: Duration,
    ) -> Result<PooledResponse, ProviderError> {
        self.send(req, timeout).await
    }

    /// The single send path: merge `extra_body`, keep `Content-Length` in sync
    /// with the (possibly merged) body, then bound only the header wait.
    async fn send(
        &self,
        mut req: reqwest::Request,
        timeout: Duration,
    ) -> Result<PooledResponse, ProviderError> {
        let method = req.method().clone();
        let url = req.url().clone();
        if let Some(extra) = &self.extra_body {
            apply_extra_body(&mut req, extra)?;
        }
        // The merge can change the body length; keep Content-Length correct on
        // the wire regardless of what the caller set.
        let body_len = req.body().and_then(|b| b.as_bytes()).map(|b| b.len());
        if let Some(len) = body_len
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&len.to_string())
        {
            req.headers_mut()
                .insert(reqwest::header::CONTENT_LENGTH, value);
        }
        let resp = execute_with_header_timeout(&self.http, req, timeout).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let debug = std::env::var(DEBUG_STREAM_ENV)
            .ok()
            .filter(|dir| !dir.is_empty())
            .and_then(|dir| {
                DebugCapture::open(Path::new(&dir), self.kind, &method, &url, status, &headers).ok()
            });
        Ok(PooledResponse {
            inner: Some(resp),
            debug,
            upstream: Arc::new(Mutex::new(UpstreamRecorder::new())),
        })
    }
}

// --- Opaque payload / JSON replay conventions ------------------------------

/// Store a provider replay payload as raw JSON bytes, parsing only for
/// validation. `Box<RawValue>` keeps the original bytes for exact replay.
pub fn preserve_opaque(bytes: &[u8]) -> Result<Box<RawValue>, ProviderError> {
    let text = std::str::from_utf8(bytes).map_err(|e| ProviderError {
        message: format!("opaque payload: {e}"),
    })?;
    RawValue::from_string(text.to_owned()).map_err(|e| ProviderError {
        message: format!("opaque payload: {e}"),
    })
}

/// Raw byte length of an opaque payload, counted toward history.
pub fn opaque_len(opaque: Option<&RawValue>) -> usize {
    opaque.map_or(0, |v| v.get().len())
}

/// Compare two JSON texts by parsed value, not by bytes. Numbers compare by
/// `f64` (so `1e0 == 1`, matching Go's float64 decoding), and string escapes
/// (`\u0061`, HTML-escapable chars) are decoded before comparing. Use this for
/// golden/differential checks instead of asserting byte-identical re-encoding.
pub fn json_parse_equivalent(a: &str, b: &str) -> bool {
    let Ok(va) = serde_json::from_str::<serde_json::Value>(a) else {
        return false;
    };
    let Ok(vb) = serde_json::from_str::<serde_json::Value>(b) else {
        return false;
    };
    value_eq(&va, &vb)
}

#[allow(clippy::float_cmp)] // Option<f64> equality groups 1e0 with 1 like Go
fn value_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Null, serde_json::Value::Null) => true,
        (serde_json::Value::Bool(x), serde_json::Value::Bool(y)) => x == y,
        (serde_json::Value::Number(x), serde_json::Value::Number(y)) => x.as_f64() == y.as_f64(),
        (serde_json::Value::String(x), serde_json::Value::String(y)) => x == y,
        (serde_json::Value::Array(x), serde_json::Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| value_eq(p, q))
        }
        (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| value_eq(v, w)))
        }
        _ => false,
    }
}

/// Map an optional usage JSON value onto [`Usage`]. An absent value stays
/// `None`; an explicit value (even all zeros) stays `Some`, distinguishing
/// "no usage reported" from "reported zero usage".
pub fn usage_from_json(value: Option<&serde_json::Value>) -> Option<Usage> {
    let v = value?;
    let field = |k: &str| v.get(k).and_then(|n| n.as_i64()).unwrap_or(0);
    Some(Usage {
        input: field("input"),
        cached: field("cached"),
        output: field("output"),
        reasoning: field("reasoning"),
        total: field("total"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_chunk_clamps_a_single_oversized_chunk() {
        // A chunk larger than the whole budget is clamped to the budget, not
        // added whole (the exact-64-KiB guarantee).
        let big = vec![7u8; DRAIN_LIMIT + 4096];
        assert_eq!(cap_chunk(&big, 0).len(), DRAIN_LIMIT);
        assert_eq!(cap_chunk(&big, 0), &big[..DRAIN_LIMIT]);
        // A chunk crossing the remaining budget is trimmed to what is left.
        let small = vec![9u8; 100];
        assert_eq!(cap_chunk(&small, DRAIN_LIMIT - 10).len(), 10);
        assert_eq!(cap_chunk(&small, DRAIN_LIMIT - 10), &small[..10]);
        // Under the cap, the whole chunk is retained.
        assert_eq!(cap_chunk(&small, 0).len(), 100);
    }
}
