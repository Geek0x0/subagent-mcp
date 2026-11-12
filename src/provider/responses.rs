//! Responses adapter (Task 11).
//!
//! Ports Go `internal/provider/responses/adapter.go` onto the Task 9
//! [`PooledClient`] send path. The request body is assembled from raw JSON
//! fragments so an assistant turn's opaque replay items keep their exact
//! bytes, and the stream is framed exactly like the OpenAI Go SDK's
//! `ssestream` decoder: `bufio.Scanner` semantics with a 32 MiB line cap,
//! blank-line dispatch, `data:` lines joined with `\n`, comment/empty events
//! skipped, and `[DONE]` ending the stream without itself counting as a
//! terminal response event.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::config::ProviderConfig;
use crate::provider::sse::SSE_MAX_LINE;
use crate::provider::transport::{PooledClient, PooledResponse, preserve_opaque};
use crate::provider::{
    DeltaSink, ModelInfo, ModelLister, Provider, ProviderError, Role, ToolCall, ToolSpec,
    TransportKind, TurnRequest, TurnResult, Usage,
};
use crate::util::run::RunContext;

/// Responses API path, appended to the configured base URL.
const RESPONSES_PATH: &str = "/responses";
/// Model-list path, appended to the configured base URL.
const MODELS_PATH: &str = "/models";
/// Reasoning items must carry their encrypted payload so a stateless
/// (`store:false`) conversation can replay them on the next turn.
const INCLUDE_REASONING_ENCRYPTED: &str = "reasoning.encrypted_content";

/// OpenAI Go SDK `MaxRetries` (default): two retries after the first attempt.
pub const MAX_RETRIES: usize = 2;
/// Total attempts allowed for one request (`MaxRetries + 1`).
pub const MAX_ATTEMPTS: usize = MAX_RETRIES + 1;
/// SDK `DefaultMaxServerDelay`: caps both the exponential backoff and any
/// server-directed `Retry-After` wait.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);
/// SDK exponential backoff base (`0.5 * 2^retryCount` seconds).
const BASE_RETRY_DELAY: Duration = Duration::from_millis(500);

/// A retry delay function `(retry_count, server_retry_after) -> delay`; tests
/// override it so retry cases do not really sleep (Go SDK `retryDelay`).
pub type RetryDelay = Arc<dyn Fn(usize, Option<Duration>) -> Duration + Send + Sync>;

static RETRY_SEED: AtomicU64 = AtomicU64::new(0);

/// Go SDK `retryDelay`: a server-directed `Retry-After` wins (capped at
/// [`MAX_RETRY_DELAY`]); otherwise `0.5s * 2^retry_count` capped at
/// [`MAX_RETRY_DELAY`], minus a uniform draw from `[0, delay/4)`.
pub fn default_retry_delay(retry_count: usize, retry_after: Option<Duration>) -> Duration {
    if let Some(delay) = retry_after {
        return delay.min(MAX_RETRY_DELAY);
    }
    let exponent = u32::try_from(retry_count).unwrap_or(u32::MAX).min(20);
    let backoff = BASE_RETRY_DELAY
        .saturating_mul(1u32 << exponent)
        .min(MAX_RETRY_DELAY);
    let jitter_range = backoff.as_nanos() / 4;
    if jitter_range == 0 {
        return backoff;
    }
    let draw = next_random() % (jitter_range as u64);
    backoff - Duration::from_nanos(draw)
}

/// A cheap splitmix64 draw stands in for `math/rand/v2` (no `rand` dependency);
/// only the retry jitter consumes it, and tests replace the whole delay.
fn next_random() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let mut state =
        RETRY_SEED.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ u64::from(nanos);
    state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    state ^ (state >> 31)
}

// --- Adapter ---------------------------------------------------------------

/// Responses provider.
pub struct Responses {
    name: String,
    /// The Task 9 adapter send path: `execute_json`/`execute` apply
    /// `extra_body` and enforce the 10-minute response-header timeout.
    client: PooledClient,
    cfg: ProviderConfig,
    api_key: String,
    retry_delay: RetryDelay,
}

impl Responses {
    /// Build a Responses adapter, resolving the API key from `cfg.key` or,
    /// when that is empty, the environment variable named by `cfg.env_key`.
    pub fn new(name: &str, cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        let api_key = match cfg.key.as_deref().filter(|key| !key.is_empty()) {
            Some(key) => key.to_string(),
            None => {
                let env_key = cfg.env_key.clone().unwrap_or_default();
                match std::env::var(&env_key) {
                    Ok(value) if !value.is_empty() => value,
                    _ => {
                        return Err(ProviderError {
                            message: format!(
                                "environment variable {env_key} (providers.{name}.env_key) must be set to the API key"
                            ),
                        });
                    }
                }
            }
        };
        Ok(Self {
            name: name.to_string(),
            client: PooledClient::from_config(TransportKind::Responses, cfg),
            cfg: cfg.clone(),
            api_key,
            retry_delay: Arc::new(default_retry_delay),
        })
    }

    /// Override the retry delay (Go SDK `retryDelay` seam for tests).
    pub fn set_retry_delay(&mut self, retry_delay: RetryDelay) {
        self.retry_delay = retry_delay;
    }

    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        let base = self.cfg.base_url.trim_end_matches('/');
        Url::parse(&format!("{base}{path}")).map_err(|e| ProviderError {
            message: format!("responses: invalid base_url: {e}"),
        })
    }

    /// The SDK's allowlisted request headers: the configured credential plus
    /// the protocol Accept header. Nothing is derived from the environment.
    fn headers(&self) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|e| {
            ProviderError {
                message: format!("responses: authorization header: {e}"),
            }
        })?;
        headers.insert(AUTHORIZATION, auth);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    /// Send one request with the OpenAI Go SDK's retry policy (`MaxRetries: 2`).
    ///
    /// A retryable failure is a connection error, a 408/409/429/5xx response,
    /// or any response whose `x-should-retry` header says `true`; `false`
    /// suppresses a status-based retry. The final response is returned to the
    /// caller (which reports the API error), while retried responses are
    /// closed first so their connections return to the pool.
    async fn send_with_retry<F>(
        &self,
        ctx: Option<&RunContext>,
        mut build: F,
    ) -> Result<PooledResponse, ProviderError>
    where
        F: FnMut() -> Result<reqwest::Request, ProviderError>,
    {
        for attempt in 0..=MAX_RETRIES {
            if let Some(ctx) = ctx
                && let Some(reason) = context_ended(ctx)
            {
                return Err(context_error(reason));
            }
            let request = build()?;
            let result = match ctx {
                Some(ctx) => tokio::select! {
                    _ = ctx.cancel.cancelled() => return Err(context_error("context canceled")),
                    _ = wait_deadline(ctx.deadline) => {
                        return Err(context_error("context deadline exceeded"));
                    }
                    result = self.client.execute(request) => result,
                },
                None => self.client.execute(request).await,
            };
            // Go returns the context error after an attempt even when that
            // attempt produced a usable response.
            if let Some(ctx) = ctx
                && let Some(reason) = context_ended(ctx)
            {
                return Err(context_error(reason));
            }
            match result {
                Ok(response) => {
                    if !should_retry_response(&response) || attempt == MAX_RETRIES {
                        return Ok(response);
                    }
                    let delay = (self.retry_delay)(attempt, retry_after(&response));
                    response.close().await;
                    if let Err(reason) = wait_retry(ctx, delay).await {
                        return Err(context_error(reason));
                    }
                }
                Err(error) => {
                    if attempt == MAX_RETRIES {
                        return Err(error);
                    }
                    let delay = (self.retry_delay)(attempt, None);
                    if let Err(reason) = wait_retry(ctx, delay).await {
                        return Err(context_error(reason));
                    }
                }
            }
        }
        unreachable!("the retry loop returns on its final attempt")
    }

    /// Build the Responses request body. `input` items are raw JSON fragments:
    /// a user/tool item is encoded here, while every assistant replay item is
    /// inserted byte-for-byte from the turn that produced it.
    fn build_body(&self, req: &TurnRequest) -> Result<Vec<u8>, ProviderError> {
        let mut input: Vec<Box<RawValue>> = Vec::new();
        for message in &req.messages {
            match message.role {
                Role::User => input.push(json_raw(serde_json::json!({
                    "content": message.text,
                    "role": "user",
                }))?),
                Role::Assistant => {
                    let payload = message.opaque.as_deref().map(RawValue::get).unwrap_or("");
                    // Go `json.Unmarshal(m.Opaque, &items)`: a missing payload
                    // is a corrupt replay, `null` is an empty item list.
                    let items: Option<Vec<Box<RawValue>>> =
                        serde_json::from_str(payload).map_err(|e| ProviderError {
                            message: format!("responses: corrupt replay payload: {e}"),
                        })?;
                    if let Some(items) = items {
                        input.extend(items);
                    }
                }
                Role::Tool => input.push(json_raw(serde_json::json!({
                    "output": message.text,
                    "call_id": message.tool_call_id.clone().unwrap_or_default(),
                    "type": "function_call_output",
                }))?),
            }
        }

        let mut tools = Vec::with_capacity(req.tools.len());
        for spec in &req.tools {
            let parameters = decode_tool_schema(spec).map_err(|e| ProviderError {
                message: format!("responses: {}", e.message),
            })?;
            tools.push(serde_json::json!({
                "type": "function",
                "name": spec.name,
                "description": spec.description,
                "parameters": parameters,
                "strict": false,
            }));
        }

        let body = WireBody {
            model: &req.model,
            input: &input,
            tools: &tools,
            store: false,
            include: [INCLUDE_REASONING_ENCRYPTED],
            stream: true,
            instructions: (!req.system.is_empty()).then_some(req.system.as_str()),
            reasoning: req
                .effort
                .as_deref()
                .filter(|effort| !effort.is_empty())
                .map(|effort| Reasoning {
                    effort,
                    summary: "auto",
                }),
            max_output_tokens: self.cfg.max_output_tokens.filter(|limit| *limit > 0),
        };
        serde_json::to_vec(&body).map_err(|e| ProviderError {
            message: format!("responses: encode request: {e}"),
        })
    }

    async fn fetch_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = self.endpoint(MODELS_PATH)?;
        let mut response = self
            .send_with_retry(None, || {
                let mut request = reqwest::Request::new(reqwest::Method::GET, url.clone());
                *request.headers_mut() = self.headers()?;
                Ok(request)
            })
            .await
            .map_err(|e| ProviderError {
                message: format!("list models: {}", e.message),
            })?;
        if let Some(status) = response.status()
            && !status.is_success()
        {
            let detail = read_body(&mut response).await;
            response.close().await;
            return Err(ProviderError {
                message: format!(
                    "list models: GET {:?}: {} {} {}",
                    url.as_str(),
                    status.as_u16(),
                    status.canonical_reason().unwrap_or(""),
                    detail
                ),
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| ProviderError {
            message: format!("list models: {}", e.message),
        })? {
            body.extend_from_slice(&chunk);
        }
        response.close().await;
        let page: ModelPage = serde_json::from_slice(&body).map_err(|e| ProviderError {
            message: format!("list models: {e}"),
        })?;
        Ok(page
            .data
            .into_iter()
            .map(|model| ModelInfo {
                id: model.id,
                description: String::new(),
            })
            .collect())
    }
}

#[async_trait::async_trait]
impl Provider for Responses {
    fn name(&self) -> &str {
        &self.name
    }

    async fn turn(
        &self,
        ctx: RunContext,
        req: TurnRequest,
        mut on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        let body = self.build_body(&req)?;
        let url = self.endpoint(RESPONSES_PATH)?;
        let mut response = self
            .send_with_retry(Some(&ctx), || {
                let mut request = reqwest::Request::new(reqwest::Method::POST, url.clone());
                *request.headers_mut() = self.headers()?;
                request
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                *request.body_mut() = Some(reqwest::Body::from(body.clone()));
                Ok(request)
            })
            .await?;

        // A non-2xx reply is a stream-setup failure, not an SSE stream. Go's
        // SDK surfaces it as `POST "<url>": <status> <reason> <body>`.
        if let Some(status) = response.status()
            && !status.is_success()
        {
            let detail = read_body(&mut response).await;
            response.close().await;
            return Err(ProviderError {
                message: format!(
                    "POST {:?}: {} {} {}",
                    url.as_str(),
                    status.as_u16(),
                    status.canonical_reason().unwrap_or(""),
                    detail
                ),
            });
        }

        let mut events = EventStream::new(response);
        let collected = events.collect(&ctx, &mut on_delta).await;
        events.close().await;
        let Some(response) = collected? else {
            return Err(ProviderError {
                message: "responses: stream ended without a terminal response event".to_string(),
            });
        };
        finish_turn(response)
    }
}

#[async_trait::async_trait]
impl ModelLister for Responses {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.fetch_models().await
    }
}

// --- Request body ----------------------------------------------------------

#[derive(Serialize)]
struct WireBody<'a> {
    model: &'a str,
    input: &'a [Box<RawValue>],
    tools: &'a [Value],
    store: bool,
    include: [&'static str; 1],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i64>,
}

#[derive(Serialize)]
struct Reasoning<'a> {
    effort: &'a str,
    summary: &'static str,
}

/// Encode one request input item as raw JSON so it can be replayed verbatim.
fn json_raw(value: Value) -> Result<Box<RawValue>, ProviderError> {
    serde_json::to_string(&value)
        .and_then(RawValue::from_string)
        .map_err(|e| ProviderError {
            message: format!("responses: encode request item: {e}"),
        })
}

/// Go `provider.DecodeToolSchema`: an empty schema is `{}`, and a payload that
/// is not a JSON object is rejected with the tool name in the error.
fn decode_tool_schema(spec: &ToolSpec) -> Result<Value, ProviderError> {
    let raw = spec.parameters.get().trim();
    if raw.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let schema: Value = serde_json::from_str(raw).map_err(|e| ProviderError {
        message: format!("tool {} schema: {e}", spec.name),
    })?;
    match schema {
        Value::Object(map) => Ok(Value::Object(map)),
        _ => Err(ProviderError {
            message: format!("tool {} schema: not a JSON object", spec.name),
        }),
    }
}

// --- Stream framing (Go `ssestream` decoder) -------------------------------

/// One dispatched SSE event: the `data:` lines joined with `\n` (each line
/// keeps its trailing newline, like the SDK). The SSE `event:` name is parsed
/// but not retained: the adapter switches on the JSON payload's own `type`.
struct SseEvent {
    data: Vec<u8>,
}

/// `bufio.Scanner`-equivalent SSE framer over a pooled response.
///
/// Lines are read with a 32 MiB cap; an empty line dispatches the accumulated
/// `data:` lines, an empty event (no data) is skipped, `:` comments, `event:`
/// names, and unknown fields are ignored, and a pending event at EOF is
/// dropped (the Go scanner never dispatches it).
struct EventStream {
    response: Option<PooledResponse>,
    buffer: BytesMut,
    eof: bool,
    data: Vec<u8>,
}

impl EventStream {
    fn new(response: PooledResponse) -> Self {
        Self {
            response: Some(response),
            buffer: BytesMut::new(),
            eof: false,
            data: Vec::new(),
        }
    }

    async fn close(mut self) {
        if let Some(response) = self.response.take() {
            response.close().await;
        }
    }

    async fn next_event(&mut self, ctx: &RunContext) -> Result<Option<SseEvent>, ProviderError> {
        loop {
            let Some(line) = self.next_line(ctx).await? else {
                return Ok(None);
            };
            if line.is_empty() {
                if self.data.is_empty() {
                    continue;
                }
                return Ok(Some(SseEvent {
                    data: std::mem::take(&mut self.data),
                }));
            }
            let (name, value) = match line.iter().position(|&b| b == b':') {
                Some(index) => (&line[..index], &line[index + 1..]),
                None => (&line[..], &line[line.len()..]),
            };
            // One optional space after the colon, like `bytes.Cut` + the SDK.
            let value = match value.first() {
                Some(b' ') => &value[1..],
                _ => value,
            };
            // Comments (`: ...`) and every other field, including `event:`,
            // are ignored: the payload's own `type` selects the event.
            if name == b"data" {
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
            }
        }
    }

    async fn next_line(&mut self, ctx: &RunContext) -> Result<Option<Vec<u8>>, ProviderError> {
        loop {
            if let Some(index) = self.buffer.iter().position(|&b| b == b'\n') {
                if index >= SSE_MAX_LINE {
                    return Err(line_too_long());
                }
                let mut line = self.buffer.split_to(index + 1);
                line.truncate(index);
                if line.last() == Some(&b'\r') {
                    let length = line.len();
                    line.truncate(length - 1);
                }
                return Ok(Some(line.to_vec()));
            }
            if self.buffer.len() >= SSE_MAX_LINE {
                return Err(line_too_long());
            }
            if self.eof {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let mut line = self.buffer.split_to(self.buffer.len());
                if line.last() == Some(&b'\r') {
                    let length = line.len();
                    line.truncate(length - 1);
                }
                return Ok(Some(line.to_vec()));
            }
            let Some(response) = self.response.as_mut() else {
                self.eof = true;
                continue;
            };
            let chunk = tokio::select! {
                _ = ctx.cancel.cancelled() => return Err(ProviderError {
                    message: "context canceled".to_string(),
                }),
                _ = wait_deadline(ctx.deadline) => return Err(ProviderError {
                    message: "context deadline exceeded".to_string(),
                }),
                chunk = response.chunk() => chunk?,
            };
            match chunk {
                Some(bytes) => self.buffer.extend_from_slice(&bytes),
                None => self.eof = true,
            }
        }
    }

    /// Read events until `[DONE]`, an error, or EOF, returning the last
    /// terminal response event (Go keeps the last one and ignores the rest).
    async fn collect(
        &mut self,
        ctx: &RunContext,
        on_delta: &mut DeltaSink,
    ) -> Result<Option<TerminalResponse>, ProviderError> {
        let mut terminal = None;
        while let Some(event) = self.next_event(ctx).await? {
            // The SDK checks `[DONE]` before parsing: it ends the stream and
            // is never a terminal response event.
            if event.data.starts_with(b"[DONE]") {
                break;
            }
            let envelope: EventEnvelope =
                serde_json::from_slice(&event.data).map_err(|e| ProviderError {
                    message: e.to_string(),
                })?;
            // The SDK rejects any event whose payload carries a top-level
            // `error` field before it decodes the event union.
            if let Some(error) = envelope.error.as_ref() {
                return Err(ProviderError {
                    message: format!("received error while streaming: {}", gjson_string(error)),
                });
            }
            match envelope.kind.as_str() {
                "response.output_text.delta" => {
                    if let Some(delta) = envelope.delta.filter(|delta| !delta.is_empty()) {
                        on_delta(delta);
                    }
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    terminal = Some(envelope.response.unwrap_or_default());
                }
                "error" => {
                    return Err(ProviderError {
                        message: format!(
                            "responses: {}: {}",
                            envelope.code.as_deref().unwrap_or(""),
                            envelope.message.as_deref().unwrap_or("")
                        ),
                    });
                }
                _ => {}
            }
        }
        Ok(terminal)
    }
}

fn line_too_long() -> ProviderError {
    ProviderError {
        message: "sse: line exceeds the maximum length".to_string(),
    }
}

/// Go `gjson.Result.String()` for a non-null value: strings unquote, every
/// other JSON value keeps its raw encoding.
fn gjson_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// --- Terminal response -----------------------------------------------------

#[derive(Deserialize, Default)]
struct EventEnvelope {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    response: Option<TerminalResponse>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// `Some` whenever the key is present, including an explicit `null`:
    /// gjson's `Exists()` is true for `"error":null`, so the SDK reports it.
    #[serde(default, deserialize_with = "present_value")]
    error: Option<Value>,
}

fn present_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, Default)]
struct TerminalResponse {
    #[serde(default)]
    status: Option<String>,
    /// Output items are kept raw so the next request can replay them verbatim.
    #[serde(default)]
    output: Option<Vec<Box<RawValue>>>,
    #[serde(default)]
    usage: Option<UsageWire>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<ResponseError>,
}

#[derive(Deserialize, Default)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct ResponseError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize, Default)]
struct UsageWire {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
    #[serde(default)]
    total_tokens: i64,
}

#[derive(Deserialize, Default)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: i64,
}

#[derive(Deserialize, Default)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: i64,
}

impl From<UsageWire> for Usage {
    fn from(usage: UsageWire) -> Self {
        Self {
            input: usage.input_tokens,
            cached: usage
                .input_tokens_details
                .map_or(0, |details| details.cached_tokens),
            output: usage.output_tokens,
            reasoning: usage
                .output_tokens_details
                .map_or(0, |details| details.reasoning_tokens),
            total: usage.total_tokens,
        }
    }
}

/// Turn a completed terminal response into the neutral result, preserving the
/// exact error text for an incomplete/failed response or a refusal part.
fn finish_turn(response: TerminalResponse) -> Result<TurnResult, ProviderError> {
    let TerminalResponse {
        status,
        output,
        usage,
        incomplete_details,
        error,
    } = response;

    match status.as_deref().unwrap_or("") {
        "completed" => {}
        "incomplete" => {
            let reason = incomplete_details
                .as_ref()
                .and_then(|details| details.reason.as_deref())
                .unwrap_or("");
            return Err(ProviderError {
                message: format!("responses: response incomplete: {reason}"),
            });
        }
        status => {
            let message = error
                .as_ref()
                .and_then(|error| error.message.as_deref())
                .unwrap_or("");
            return Err(ProviderError {
                message: format!("responses: response {status}: {message}"),
            });
        }
    }

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut reasoning = Vec::new();
    let mut raw_items: Vec<Box<RawValue>> = Vec::new();
    for item in output.unwrap_or_default() {
        let view: Value = serde_json::from_str(item.get()).map_err(|e| ProviderError {
            message: format!("responses: decode output item: {e}"),
        })?;
        raw_items.push(item);
        match view.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                let parts = view
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                for part in parts {
                    match part.get("type").and_then(Value::as_str).unwrap_or("") {
                        "output_text" => {
                            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""))
                        }
                        "refusal" => {
                            // A refusal is a completed response with no answer;
                            // returning it as an empty success would end the
                            // session with nothing, so fail like the Messages
                            // adapter does on a refusal stop reason.
                            let refusal = part.get("refusal").and_then(Value::as_str).unwrap_or("");
                            return Err(ProviderError {
                                message: format!("responses: model refused: {refusal}"),
                            });
                        }
                        _ => {}
                    }
                }
            }
            "function_call" => tool_calls.push(ToolCall {
                id: view
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: view
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                arguments: view
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }),
            "reasoning" => {
                let summaries = view
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                for summary in summaries {
                    reasoning.push(
                        summary
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    );
                }
            }
            _ => {}
        }
    }

    let reasoning = reasoning.join("\n");
    let opaque = if raw_items.is_empty() {
        RawValue::from_string("[]".to_string()).map_err(|e| ProviderError {
            message: format!("responses: encode replay payload: {e}"),
        })?
    } else {
        let encoded = serde_json::to_string(&raw_items).map_err(|e| ProviderError {
            message: format!("responses: encode replay payload: {e}"),
        })?;
        preserve_opaque(encoded.as_bytes())?
    };

    Ok(TurnResult {
        text,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        tool_calls,
        // Go always reports usage from the terminal response, zeroed when the
        // server omitted it.
        usage: Some(usage.map(Usage::from).unwrap_or_default()),
        opaque: Some(opaque),
        // Go's responses adapter never reads the chat-completions upstream
        // recorder, so this stays empty.
        upstream: None,
    })
}

// --- Retry policy (Go SDK `requestconfig`) --------------------------------

/// Go `shouldRetry` for a response: an explicit `x-should-retry` header wins
/// over the status; otherwise only 408/409/429/5xx are transient. A 2xx with
/// `x-should-retry: true` is retried too, exactly like the SDK.
fn should_retry_response(response: &PooledResponse) -> bool {
    if let Some(value) = response
        .headers()
        .and_then(|headers| headers.get("x-should-retry"))
        .and_then(|value| value.to_str().ok())
    {
        if value == "true" {
            return true;
        }
        if value == "false" {
            return false;
        }
    }
    response.status().is_some_and(retryable_status)
}

fn retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error()
}

/// Go SDK `parseRetryAfterHeader`: `Retry-After-Ms` first, then `Retry-After`
/// (seconds or an HTTP date), capped at [`MAX_RETRY_DELAY`]. An unparsable or
/// negative value falls through to the exponential backoff.
fn retry_after(response: &PooledResponse) -> Option<Duration> {
    let headers = response.headers()?;
    for (name, unit_seconds) in [("retry-after-ms", 0.001), ("retry-after", 1.0)] {
        let Some(value) = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if let Ok(seconds) = value.parse::<f64>() {
            if !seconds.is_finite() || seconds < 0.0 {
                continue;
            }
            let cap = MAX_RETRY_DELAY.as_secs_f64() / unit_seconds;
            if seconds >= cap {
                return Some(MAX_RETRY_DELAY);
            }
            return Some(Duration::from_secs_f64(seconds * unit_seconds));
        }
        if name == "retry-after"
            && let Ok(when) = chrono::DateTime::parse_from_rfc2822(value)
        {
            let remaining = when.with_timezone(&chrono::Utc) - chrono::Utc::now();
            let delay = remaining.to_std().unwrap_or(Duration::ZERO);
            return Some(delay.min(MAX_RETRY_DELAY));
        }
    }
    None
}

/// Sleep for a retry delay, returning the context reason if the run ends first.
async fn wait_retry(ctx: Option<&RunContext>, delay: Duration) -> Result<(), &'static str> {
    let Some(ctx) = ctx else {
        tokio::time::sleep(delay).await;
        return Ok(());
    };
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = ctx.cancel.cancelled() => Err("context canceled"),
        _ = wait_deadline(ctx.deadline) => Err("context deadline exceeded"),
    }
}

fn context_error(reason: &'static str) -> ProviderError {
    ProviderError {
        message: reason.to_string(),
    }
}

// --- Model listing ---------------------------------------------------------

#[derive(Deserialize, Default)]
struct ModelPage {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize, Default)]
struct ModelEntry {
    #[serde(default)]
    id: String,
}

// --- Shared helpers --------------------------------------------------------

async fn read_body(response: &mut PooledResponse) -> String {
    let mut buffer = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        buffer.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

fn context_ended(ctx: &RunContext) -> Option<&'static str> {
    if ctx.cancel.is_cancelled() {
        return Some("context canceled");
    }
    if let Some(deadline) = ctx.deadline
        && tokio::time::Instant::now() >= deadline
    {
        return Some("context deadline exceeded");
    }
    None
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}
