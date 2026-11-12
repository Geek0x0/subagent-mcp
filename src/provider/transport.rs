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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::value::RawValue;

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
pub fn shared_client(kind: TransportKind, base_url: &str) -> reqwest::Client {
    let key = format!("{}\x00{}", kind_label(kind), base_url);
    let mut clients = CLIENTS.lock().unwrap();
    if let Some(client) = clients.get(&key) {
        return client.clone();
    }
    let client = build_client();
    clients.insert(key, client.clone());
    client
}

fn build_client() -> reqwest::Client {
    let tls = crate::provider::tls::client_config()
        .expect("provider transport: platform TLS client configuration unavailable");
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        // Match Go's http.DefaultTransport dial (30 s) and idle (90 s) bounds.
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("provider transport: build reqwest client")
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

/// Drain an unread response body up to [`DRAIN_LIMIT`] bytes and
/// [`DRAIN_TIMEOUT`], teeing chunks to the debug capture and upstream recorder.
/// Returns the number of bytes drained. This is the core of the drain-on-drop
/// behavior: a sentinel-but-still-open stream cannot stall the caller.
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
                        total += bytes.len();
                        if let Some(d) = debug.as_mut() {
                            d.write_chunk(&bytes);
                        }
                        upstream.lock().unwrap().feed(&bytes);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        };
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
    }
    total
}

/// Drain a bare `reqwest::Response` with the standard bounds. Exposed so the
/// bounded-drain contract is directly testable.
pub async fn drain_response(resp: reqwest::Response) -> usize {
    bounded_drain(resp, None, Arc::new(Mutex::new(UpstreamRecorder::new()))).await
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

/// A client over the shared pool that wraps responses with debug capture,
/// upstream recording, and bounded drain-on-drop. Adapters call [`Self::execute`]
/// after applying [`merge_extra_body`] to their request body.
pub struct PooledClient {
    http: reqwest::Client,
    kind: TransportKind,
}

impl PooledClient {
    pub fn new(kind: TransportKind, base_url: &str) -> Self {
        Self {
            http: shared_client(kind, base_url),
            kind,
        }
    }

    /// Execute `req` with the header-only timeout and wrap the response.
    pub async fn execute(
        &self,
        req: reqwest::Request,
        timeout: Duration,
    ) -> Result<PooledResponse, ProviderError> {
        let method = req.method().clone();
        let url = req.url().clone();
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
