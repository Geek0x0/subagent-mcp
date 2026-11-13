//! Anthropic Messages adapter (Task 12).
//!
//! Ports Go `internal/provider/messages/adapter.go` onto the Task 9
//! [`PooledClient`] send path. Requests carry the adaptive summarized thinking
//! configuration, the top-level ephemeral cache control, the configured output
//! effort, and every tool-schema keyword the caller declared. Assistant turns
//! are stored as an opaque replay payload and replayed block for block, with
//! empty text blocks dropped because the Messages API rejects them.
//!
//! The stream uses the Messages framing rules from the design: blank-line
//! dispatch, a 32 MiB line cap, multiple `data:` lines concatenated with `\n`,
//! `[DONE]` as ordinary data, and a non-empty `stop_reason` as the terminal
//! signal (`message_stop` is not required).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use reqwest::{StatusCode, Url};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::ProviderConfig;
use crate::provider::sse::SSE_MAX_LINE;
use crate::provider::transport::{PooledClient, PooledResponse};
use crate::provider::{
    DeltaSink, Message, ModelInfo, ModelLister, Provider, ProviderError, Role, ToolCall, ToolSpec,
    TransportKind, TurnRequest, TurnResult, Usage,
};
use crate::util::run::RunContext;

/// Bounds `pause_turn` re-requests within one turn (Go `maxPauseContinuations`).
pub const MAX_PAUSE_CONTINUATIONS: usize = 5;

/// Go SDK `MaxRetries: 2`: three total attempts for a retryable request.
pub const MAX_RETRIES: usize = 2;

const MESSAGES_PATH: &str = "v1/messages";
const MODELS_PATH: &str = "v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A retry delay function `(attempt, Retry-After) -> Duration`. Tests override
/// it so a retryable response does not really sleep.
pub type RetryDelay = Arc<dyn Fn(usize, Option<Duration>) -> Duration + Send + Sync>;

static RETRY_SEED: AtomicU64 = AtomicU64::new(0);

/// Go SDK `retryDelay`: the server's `Retry-After` when present, otherwise
/// `0.5s * 2^attempt` capped at 8s with up to a quarter of it jittered away. A
/// cheap time+seed mix stands in for `math/rand` (no `rand` dependency).
pub fn default_retry_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    if let Some(retry_after) = retry_after {
        return retry_after;
    }
    let max_delay = Duration::from_secs(8);
    let base = Duration::from_millis(500).saturating_mul(1u32 << attempt.min(4) as u32);
    let delay = base.min(max_delay);
    let quarter = delay / 4;
    if quarter.is_zero() {
        return delay;
    }
    let span = quarter.as_nanos() as u64 + 1;
    let seed = RETRY_SEED.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
        ^ u64::from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos(),
        );
    delay - Duration::from_nanos(seed % span)
}

/// Anthropic Messages provider.
pub struct Messages {
    name: String,
    /// The Task 9 adapter send path: `execute_json`/`execute` apply
    /// `extra_body` and enforce the 10-minute response-header timeout.
    client: PooledClient,
    cfg: ProviderConfig,
    api_key: String,
    max_tokens: i64,
    retry_delay: RetryDelay,
}

impl Messages {
    /// Build a Messages adapter, resolving the API key from `cfg.key` or, when
    /// that is empty, the environment variable named by `cfg.env_key`.
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
            client: PooledClient::from_config(TransportKind::Messages, cfg)?,
            max_tokens: cfg.max_output_tokens.unwrap_or(0),
            cfg: cfg.clone(),
            api_key,
            retry_delay: Arc::new(default_retry_delay),
        })
    }

    /// Override the retry delay (tests use this so retries do not really sleep).
    pub fn set_retry_delay(&mut self, delay: RetryDelay) {
        self.retry_delay = delay;
    }

    /// Resolve an API path the way the Go SDK does: relative to the configured
    /// base URL, so a base URL without a trailing slash replaces its last path
    /// segment exactly as Go's `url.URL.Parse` would.
    fn endpoint(&self, path: &str) -> Result<Url, ProviderError> {
        let base = Url::parse(&self.cfg.base_url).map_err(|error| ProviderError {
            message: format!(
                "messages: invalid base URL {:?}: {error}",
                self.cfg.base_url
            ),
        })?;
        base.join(path).map_err(|error| ProviderError {
            message: format!(
                "messages: invalid base URL {:?}: {error}",
                self.cfg.base_url
            ),
        })
    }

    /// Protocol headers the SDK sets on every request. No environment-derived
    /// credential or header ever reaches the wire.
    fn headers(&self) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        let key = HeaderValue::from_str(&self.api_key).map_err(|error| ProviderError {
            message: format!("messages: api key header: {error}"),
        })?;
        headers.insert("x-api-key", key);
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    /// Send the turn request with the Go SDK retry policy (`MaxRetries: 2`):
    /// 408/409/429/5xx, `x-should-retry: true`, and connection errors retry;
    /// other 4xx and cancellation/deadline do not.
    async fn send_json_with_retry(
        &self,
        ctx: &RunContext,
        url: &Url,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<PooledResponse, ProviderError> {
        let mut attempt = 0usize;
        loop {
            if let Some(reason) = context_ended(ctx) {
                return Err(ProviderError {
                    message: reason.to_string(),
                });
            }
            match self.send_json_once(ctx, url, headers, body, attempt).await {
                SendOutcome::Success(response) => return Ok(response),
                SendOutcome::Failed(failure) => {
                    attempt = self.retry_or_fail(ctx, attempt, failure).await?;
                }
            }
        }
    }

    /// Send a `GET` request (model listing) with the same retry policy.
    async fn send_get_with_retry(
        &self,
        ctx: &RunContext,
        url: &Url,
        headers: &HeaderMap,
    ) -> Result<PooledResponse, ProviderError> {
        let mut attempt = 0usize;
        loop {
            if let Some(reason) = context_ended(ctx) {
                return Err(ProviderError {
                    message: reason.to_string(),
                });
            }
            match self.send_get_once(ctx, url, headers, attempt).await {
                SendOutcome::Success(response) => return Ok(response),
                SendOutcome::Failed(failure) => {
                    attempt = self.retry_or_fail(ctx, attempt, failure).await?;
                }
            }
        }
    }

    async fn send_json_once(
        &self,
        ctx: &RunContext,
        url: &Url,
        headers: &HeaderMap,
        body: &[u8],
        attempt: usize,
    ) -> SendOutcome {
        tokio::select! {
            response = self.client.execute_json(url.clone(), headers.clone(), body) => {
                match response {
                    Ok(response) => self.classify("POST", url, response, attempt).await,
                    Err(error) => SendOutcome::Failed(AttemptFailure::Connection(error.message)),
                }
            }
            _ = ctx.cancel.cancelled() => {
                SendOutcome::Failed(AttemptFailure::Context("context canceled"))
            }
            _ = wait_deadline(ctx.deadline) => {
                SendOutcome::Failed(AttemptFailure::Context("context deadline exceeded"))
            }
        }
    }

    async fn send_get_once(
        &self,
        ctx: &RunContext,
        url: &Url,
        headers: &HeaderMap,
        attempt: usize,
    ) -> SendOutcome {
        let mut request = reqwest::Request::new(reqwest::Method::GET, url.clone());
        *request.headers_mut() = headers.clone();
        tokio::select! {
            response = self.client.execute(request) => {
                match response {
                    Ok(response) => self.classify("GET", url, response, attempt).await,
                    Err(error) => SendOutcome::Failed(AttemptFailure::Connection(error.message)),
                }
            }
            _ = ctx.cancel.cancelled() => {
                SendOutcome::Failed(AttemptFailure::Context("context canceled"))
            }
            _ = wait_deadline(ctx.deadline) => {
                SendOutcome::Failed(AttemptFailure::Context("context deadline exceeded"))
            }
        }
    }

    /// Classify one response. A retryable failure is closed **unread**, like Go
    /// (`res.Body.Close()` before retrying): reading a slow or stalled error
    /// body here would delay the retry and mask the `Retry-After` delay. Only a
    /// failure that ends the turn reads its body for the error text.
    async fn classify(
        &self,
        method: &str,
        url: &Url,
        mut response: PooledResponse,
        attempt: usize,
    ) -> SendOutcome {
        let status = response
            .status()
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status.is_success() {
            return SendOutcome::Success(response);
        }
        let headers = response.headers().cloned().unwrap_or_default();
        if should_retry(status.as_u16(), &headers) && attempt < MAX_RETRIES {
            // Dropping the response runs the transport's bounded background
            // drain (64 KiB / 250 ms) without blocking the retry.
            drop(response);
            return SendOutcome::Failed(AttemptFailure::Retry {
                retry_after: parse_retry_after(&headers),
            });
        }
        let body = read_body(&mut response).await;
        response.close().await;
        SendOutcome::Failed(AttemptFailure::Failure(ProviderError {
            message: api_error(method, url, status, &body),
        }))
    }

    /// Either return the terminal error or wait out the retry delay and return
    /// the next attempt index.
    async fn retry_or_fail(
        &self,
        ctx: &RunContext,
        attempt: usize,
        failure: AttemptFailure,
    ) -> Result<usize, ProviderError> {
        match failure {
            AttemptFailure::Context(reason) => Err(ProviderError {
                message: reason.to_string(),
            }),
            AttemptFailure::Failure(error) => Err(error),
            AttemptFailure::Retry { retry_after } => {
                let delay = (self.retry_delay)(attempt, retry_after);
                wait_for_backoff(ctx, delay).await?;
                Ok(attempt + 1)
            }
            AttemptFailure::Connection(message) => {
                if attempt >= MAX_RETRIES {
                    return Err(ProviderError { message });
                }
                let delay = (self.retry_delay)(attempt, None);
                wait_for_backoff(ctx, delay).await?;
                Ok(attempt + 1)
            }
        }
    }

    /// Accumulate one message from the SSE stream. `on_delta` receives text
    /// deltas as they arrive.
    async fn stream(
        &self,
        ctx: &RunContext,
        params: &Params,
        on_delta: &mut DeltaSink,
    ) -> Result<Accumulator, ProviderError> {
        let url = self.endpoint(MESSAGES_PATH)?;
        let body = serde_json::to_vec(&params.to_json()).map_err(|error| ProviderError {
            message: format!("messages: encode request: {error}"),
        })?;
        let headers = self.headers()?;

        let mut response = self
            .send_json_with_retry(ctx, &url, &headers, &body)
            .await?;
        let status = response.status().unwrap_or(StatusCode::OK);

        let mut accumulator = Accumulator::default();
        let mut framer = Framer::default();
        let mut buffer: Vec<u8> = Vec::new();
        let mut eof = false;
        while let Some(line) = next_line(&mut response, &mut buffer, &mut eof, ctx).await? {
            let Some(event) = framer.push_line(&line) else {
                continue;
            };
            handle_event(&mut accumulator, &event, on_delta, &url, status)?;
        }

        // A stream that ends before `message_delta` carries no stop signal: it
        // is a truncated reply, not an answer. `message_stop` is not required.
        if accumulator.stop_reason.is_empty() {
            return Err(ProviderError {
                message: format!(
                    "messages: stream ended without a terminal stop signal (partial text: {:?})",
                    truncate_runes(&accumulator.streamed, 120)
                ),
            });
        }
        Ok(accumulator)
    }
}

#[async_trait::async_trait]
impl Provider for Messages {
    fn name(&self) -> &str {
        &self.name
    }

    async fn turn(
        &self,
        ctx: RunContext,
        req: TurnRequest,
        mut on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        let mut params = Params {
            model: req.model,
            max_tokens: self.max_tokens,
            system: req.system,
            // Go only sets `output_config` when the effort is non-empty.
            effort: req.effort.filter(|effort| !effort.is_empty()),
            messages: build_messages(&req.messages)?,
            tools: build_tools(&req.tools)?,
        };

        let mut turn: Vec<Accumulator> = Vec::new();
        let mut attempt = 0usize;
        loop {
            let message = self.stream(&ctx, &params, &mut on_delta).await?;
            let paused = message.stop_reason == "pause_turn";
            turn.push(message);
            if !paused || attempt >= MAX_PAUSE_CONTINUATIONS {
                break;
            }
            let last = turn.last().expect("a message was just accumulated");
            if let Some(param) = replay_message(&last.to_stored_json()) {
                params.messages.push(param);
            }
            attempt += 1;
        }

        let last = turn.last().expect("a message was accumulated");
        if last.stop_reason == "pause_turn" {
            return Err(ProviderError {
                message: format!(
                    "messages: turn still paused after {MAX_PAUSE_CONTINUATIONS} continuations"
                ),
            });
        }
        match last.stop_reason.as_str() {
            "refusal" => {
                let details = last.stop_details.as_ref();
                let category = details
                    .and_then(|details| details.get("category"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let explanation = details
                    .and_then(|details| details.get("explanation"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                return Err(ProviderError {
                    message: format!(
                        "messages: model refused the request (category {category:?}): {explanation}"
                    ),
                });
            }
            "max_tokens" => {
                return Err(ProviderError {
                    message: "messages: response hit max_tokens; raise max_output_tokens in the provider config"
                        .to_string(),
                });
            }
            "model_context_window_exceeded" => {
                return Err(ProviderError {
                    message: "messages: response hit model_context_window_exceeded; shorten the conversation or use a model with a larger context window"
                        .to_string(),
                });
            }
            _ => {}
        }

        to_result(&turn)
    }
}

#[async_trait::async_trait]
impl ModelLister for Messages {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // The trait carries no RunContext, so retries run with a context that
        // can never be cancelled (Go passes the caller's ctx).
        let ctx = RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        };
        let headers = self.headers()?;
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut url = self.endpoint(MODELS_PATH)?;
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
            let mut response = self
                .send_get_with_retry(&ctx, &url, &headers)
                .await
                .map_err(|error| ProviderError {
                    message: format!("list models: {}", error.message),
                })?;
            let body = read_body(&mut response).await;
            response.close().await;
            let page: Value = serde_json::from_slice(&body).map_err(|error| ProviderError {
                message: format!("list models: {error}"),
            })?;
            if let Some(data) = page.get("data").and_then(Value::as_array) {
                for entry in data {
                    if let Some(id) = entry.get("id").and_then(Value::as_str) {
                        models.push(ModelInfo {
                            id: id.to_string(),
                            description: String::new(),
                        });
                    }
                }
            }
            let has_more = page
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let last_id = page
                .get("last_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string);
            match (has_more, last_id) {
                (true, Some(last)) => after = Some(last),
                _ => break,
            }
        }
        Ok(models)
    }
}

// --- Request parameters ----------------------------------------------------

/// One Messages API request body. Rebuilt for each pause-turn continuation with
/// the previous assistant message appended.
struct Params {
    model: String,
    max_tokens: i64,
    system: String,
    effort: Option<String>,
    messages: Vec<Value>,
    tools: Vec<Value>,
}

impl Params {
    fn to_json(&self) -> Value {
        let mut body = Map::new();
        body.insert("model".to_string(), Value::String(self.model.clone()));
        body.insert("max_tokens".to_string(), json!(self.max_tokens));
        // `buildMessages` returns a nil slice for empty history (omitted); every
        // real turn carries at least one message.
        if !self.messages.is_empty() {
            body.insert("messages".to_string(), Value::Array(self.messages.clone()));
        }
        // `buildTools` always returns a non-nil slice, so `tools` is emitted
        // even when empty (Go's `omitzero` keeps non-nil empty slices).
        body.insert("tools".to_string(), Value::Array(self.tools.clone()));
        body.insert(
            "thinking".to_string(),
            json!({"type": "adaptive", "display": "summarized"}),
        );
        body.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        body.insert("stream".to_string(), json!(true));
        if !self.system.is_empty() {
            body.insert(
                "system".to_string(),
                json!([{"type": "text", "text": self.system}]),
            );
        }
        if let Some(effort) = &self.effort {
            body.insert("output_config".to_string(), json!({"effort": effort}));
        }
        Value::Object(body)
    }
}

/// Replay neutral history; consecutive tool results become one user message.
fn build_messages(history: &[Message]) -> Result<Vec<Value>, ProviderError> {
    let mut out = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    for message in history {
        match message.role {
            Role::Tool => results.push(json!({
                "type": "tool_result",
                "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                "content": [{"type": "text", "text": message.text}],
                "is_error": message.is_error,
            })),
            Role::User => {
                flush_results(&mut out, &mut results);
                out.push(json!({
                    "role": "user",
                    "content": [{"type": "text", "text": message.text}],
                }));
            }
            Role::Assistant => {
                flush_results(&mut out, &mut results);
                let opaque = message.opaque.as_ref().map_or("", |raw| raw.get());
                let stored: Vec<Value> =
                    serde_json::from_str(opaque).map_err(|error| ProviderError {
                        message: format!("messages: corrupt replay payload: {error}"),
                    })?;
                for stored_message in &stored {
                    if let Some(param) = replay_message(stored_message) {
                        out.push(param);
                    }
                }
            }
        }
    }
    flush_results(&mut out, &mut results);
    Ok(out)
}

fn flush_results(out: &mut Vec<Value>, results: &mut Vec<Value>) {
    if !results.is_empty() {
        out.push(json!({"role": "user", "content": Value::Array(std::mem::take(results))}));
    }
}

/// Convert one stored assistant message into a request message, dropping empty
/// text blocks. A message with nothing sendable left — an empty content array
/// from a legal empty end_turn reply, or only empty text blocks — is not
/// replayed at all: the Messages API rejects any message whose content is empty.
fn replay_message(stored: &Value) -> Option<Value> {
    let role = stored.get("role").and_then(Value::as_str).unwrap_or("");
    let mut blocks = Vec::new();
    if let Some(content) = stored.get("content").and_then(Value::as_array) {
        for block in content {
            if is_empty_text_block(block) {
                continue;
            }
            blocks.push(block.clone());
        }
    }
    if blocks.is_empty() {
        return None;
    }
    Some(json!({"role": role, "content": blocks}))
}

fn is_empty_text_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
}

/// Build tool definitions, forwarding every schema keyword the caller declared.
fn build_tools(specs: &[ToolSpec]) -> Result<Vec<Value>, ProviderError> {
    let mut tools = Vec::with_capacity(specs.len());
    for spec in specs {
        let schema = decode_tool_schema(spec)?;
        let mut input = Map::new();
        input.insert("type".to_string(), json!("object"));
        if let Some(properties) = schema.get("properties").filter(|value| !value.is_null()) {
            input.insert("properties".to_string(), properties.clone());
        }
        if let Some(required) = schema_required(&spec.name, schema.get("required"))? {
            input.insert(
                "required".to_string(),
                Value::Array(required.into_iter().map(Value::String).collect()),
            );
        }
        // Every other keyword — additionalProperties, $defs, oneOf, title — is
        // forwarded as written. "type" is not forwarded because the schema is
        // always sent as an object.
        for (key, value) in &schema {
            match key.as_str() {
                "properties" | "required" | "type" => {}
                _ => {
                    input.insert(key.clone(), value.clone());
                }
            }
        }
        tools.push(json!({
            "name": spec.name,
            "description": spec.description,
            "input_schema": Value::Object(input),
            "eager_input_streaming": true,
        }));
    }
    Ok(tools)
}

fn decode_tool_schema(spec: &ToolSpec) -> Result<Map<String, Value>, ProviderError> {
    let value: Value =
        serde_json::from_str(spec.parameters.get()).map_err(|error| ProviderError {
            message: format!("messages: tool {} schema: {error}", spec.name),
        })?;
    match value {
        Value::Object(schema) => Ok(schema),
        _ => Err(ProviderError {
            message: format!("messages: tool {} schema: not a JSON object", spec.name),
        }),
    }
}

/// Read a schema's `required` list. The Messages API takes it as a list of
/// names, so anything else is reported rather than dropped.
fn schema_required(tool: &str, raw: Option<&Value>) -> Result<Option<Vec<String>>, ProviderError> {
    let Some(raw) = raw.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(items) = raw.as_array() else {
        return Err(ProviderError {
            message: format!("messages: tool {tool} schema: required must be an array of names"),
        });
    };
    let mut required = Vec::with_capacity(items.len());
    for item in items {
        let Some(name) = item.as_str() else {
            return Err(ProviderError {
                message: format!("messages: tool {tool} schema: required must list only names"),
            });
        };
        required.push(name.to_string());
    }
    Ok(Some(required))
}

// --- Stream accumulation ---------------------------------------------------

/// Per-message token counts as reported by the API.
#[derive(Clone, Copy, Default)]
struct UsageCounts {
    input: i64,
    cache_read: i64,
    cache_creation: i64,
    output: i64,
    thinking: i64,
}

impl UsageCounts {
    fn from_message(message: &Map<String, Value>) -> Self {
        let usage = message.get("usage");
        let field = |name: &str| {
            usage
                .and_then(|usage| usage.get(name))
                .and_then(Value::as_i64)
                .unwrap_or(0)
        };
        Self {
            input: field("input_tokens"),
            cache_read: field("cache_read_input_tokens"),
            cache_creation: field("cache_creation_input_tokens"),
            output: field("output_tokens"),
            thinking: usage
                .and_then(|usage| usage.get("output_tokens_details"))
                .and_then(|details| details.get("thinking_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
        }
    }

    /// `message_delta` reports cumulative totals: `output_tokens` always
    /// overwrites, and the other counts overwrite only when present.
    fn apply_delta(&mut self, usage: &Value) {
        self.output = usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_i64) {
            self.input = value;
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_i64) {
            self.cache_read = value;
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
        {
            self.cache_creation = value;
        }
        if let Some(details) = usage
            .get("output_tokens_details")
            .filter(|details| !details.is_null())
        {
            self.thinking = details
                .get("thinking_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
        }
    }
}

enum BlockKind {
    Text {
        text: String,
        citations: Vec<Value>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    ToolUse {
        /// Raw `input_json_delta` text, kept verbatim so a truncated tool call
        /// reaches the runner as invalid JSON.
        input: String,
    },
    Other,
}

struct StoredBlock {
    raw: Map<String, Value>,
    kind: BlockKind,
}

impl StoredBlock {
    fn new(mut raw: Map<String, Value>) -> Self {
        let kind = match raw.get("type").and_then(Value::as_str) {
            Some("text") => BlockKind::Text {
                text: raw
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                citations: Vec::new(),
            },
            Some("thinking") => BlockKind::Thinking {
                thinking: raw
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                signature: raw
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
            Some("tool_use") => {
                let input = match raw.get("input") {
                    Some(value) => {
                        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                    }
                    None => {
                        raw.insert("input".to_string(), json!({}));
                        "{}".to_string()
                    }
                };
                BlockKind::ToolUse { input }
            }
            _ => BlockKind::Other,
        };
        Self { raw, kind }
    }

    /// A text block that received no text: dropped from the stored replay
    /// payload because the API rejects empty text blocks.
    fn is_empty_text(&self) -> bool {
        matches!(&self.kind, BlockKind::Text { text, .. } if text.is_empty())
    }

    /// Overlay the delta-mutated fields onto the block's wire JSON, leaving a
    /// block that received no deltas byte for byte.
    fn refresh(&mut self) {
        match &self.kind {
            BlockKind::Text { text, citations } => {
                if !text.is_empty() {
                    self.raw
                        .insert("text".to_string(), Value::String(text.clone()));
                }
                if !citations.is_empty() {
                    self.raw
                        .insert("citations".to_string(), Value::Array(citations.clone()));
                }
            }
            BlockKind::Thinking {
                thinking,
                signature,
            } => {
                if !thinking.is_empty() {
                    self.raw
                        .insert("thinking".to_string(), Value::String(thinking.clone()));
                }
                if !signature.is_empty() {
                    self.raw
                        .insert("signature".to_string(), Value::String(signature.clone()));
                }
            }
            BlockKind::ToolUse { input } => match serde_json::from_str::<Value>(input) {
                Ok(value) => {
                    self.raw.insert("input".to_string(), value);
                }
                Err(_) if !input.is_empty() => {
                    // A cut-off tool call left non-JSON input; empty it so the
                    // block can be replayed.
                    self.raw.insert("input".to_string(), json!({}));
                }
                Err(_) => {}
            },
            BlockKind::Other => {}
        }
    }
}

#[derive(Default)]
struct Accumulator {
    message: Map<String, Value>,
    blocks: Vec<StoredBlock>,
    stop_reason: String,
    stop_details: Option<Value>,
    usage: UsageCounts,
    /// Text deltas as streamed, for the truncated-stream error.
    streamed: String,
}

impl Accumulator {
    fn accumulate(&mut self, data: &Value, on_delta: &mut DeltaSink) -> Result<(), ProviderError> {
        let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "message_start" => {
                self.message = data
                    .get("message")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                self.blocks.clear();
                self.stop_reason.clear();
                self.stop_details = None;
                self.streamed.clear();
                self.usage = UsageCounts::from_message(&self.message);
            }
            "message_delta" => {
                let delta = data.get("delta");
                self.stop_reason = delta
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.stop_details = delta
                    .and_then(|delta| delta.get("stop_details"))
                    .filter(|details| !details.is_null())
                    .cloned();
                self.usage
                    .apply_delta(data.get("usage").unwrap_or(&Value::Null));
            }
            "content_block_start" => {
                let index = data.get("index").and_then(Value::as_i64).unwrap_or(-1);
                if index != self.blocks.len() as i64 {
                    return Err(ProviderError {
                        message: format!(
                            "received event of type {event_type} for content block at index {index}, expected index {}",
                            self.blocks.len()
                        ),
                    });
                }
                let raw = data
                    .get("content_block")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                self.blocks.push(StoredBlock::new(raw));
            }
            "content_block_delta" => {
                let index = self.block_index(data, event_type)?;
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
                let block = &mut self.blocks[index];
                match delta_type {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if let BlockKind::Text {
                            text: block_text, ..
                        } = &mut block.kind
                        {
                            block_text.push_str(text);
                        }
                        if !text.is_empty() {
                            self.streamed.push_str(text);
                            on_delta(text.to_string());
                        }
                    }
                    "input_json_delta" => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !partial.is_empty()
                            && let BlockKind::ToolUse { input } = &mut block.kind
                        {
                            // A first delta replaces the "{}" placeholder.
                            if input == "{}" {
                                input.clear();
                            }
                            input.push_str(partial);
                        }
                    }
                    "thinking_delta" => {
                        let thinking = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if let BlockKind::Thinking {
                            thinking: block_thinking,
                            ..
                        } = &mut block.kind
                        {
                            block_thinking.push_str(thinking);
                        }
                    }
                    "signature_delta" => {
                        let signature =
                            delta.get("signature").and_then(Value::as_str).unwrap_or("");
                        if let BlockKind::Thinking {
                            signature: block_signature,
                            ..
                        } = &mut block.kind
                        {
                            block_signature.push_str(signature);
                        }
                    }
                    "citations_delta" => {
                        if let BlockKind::Text { citations, .. } = &mut block.kind {
                            citations.push(delta.get("citation").cloned().unwrap_or(Value::Null));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = self.block_index(data, event_type)?;
                self.blocks[index].refresh();
            }
            "message_stop" => {
                for block in &mut self.blocks {
                    block.refresh();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn block_index(&self, data: &Value, event_type: &str) -> Result<usize, ProviderError> {
        let index = data.get("index").and_then(Value::as_i64).unwrap_or(-1);
        if index < 0 || index >= self.blocks.len() as i64 {
            return Err(ProviderError {
                message: format!(
                    "received event of type {event_type} for content block at index {index} but there are only {} content blocks",
                    self.blocks.len()
                ),
            });
        }
        Ok(index as usize)
    }

    /// The stored replay message: the `message_start` object with the
    /// delta-refreshed content blocks and terminal stop information. Empty text
    /// blocks are dropped here, mirroring Go `replayTurn`: such a block can only
    /// come from a stream cut before its `content_block_stop`, and the API
    /// rejects it on replay.
    fn to_stored_json(&self) -> Value {
        let mut message = self.message.clone();
        message.insert(
            "role".to_string(),
            Value::String(
                message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("assistant")
                    .to_string(),
            ),
        );
        message.insert(
            "content".to_string(),
            Value::Array(
                self.blocks
                    .iter()
                    .filter(|block| !block.is_empty_text())
                    .map(|block| Value::Object(block.raw.clone()))
                    .collect(),
            ),
        );
        message.insert(
            "stop_reason".to_string(),
            Value::String(self.stop_reason.clone()),
        );
        if let Some(details) = &self.stop_details {
            message.insert("stop_details".to_string(), details.clone());
        }
        message.insert(
            "usage".to_string(),
            json!({
                "input_tokens": self.usage.input,
                "cache_read_input_tokens": self.usage.cache_read,
                "cache_creation_input_tokens": self.usage.cache_creation,
                "output_tokens": self.usage.output,
                "output_tokens_details": {"thinking_tokens": self.usage.thinking},
            }),
        );
        Value::Object(message)
    }
}

/// Map the accumulated turn onto the provider-neutral result.
fn to_result(turn: &[Accumulator]) -> Result<TurnResult, ProviderError> {
    let mut text = String::new();
    let mut reasoning: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();
    let mut usage = Usage::default();
    for message in turn {
        for block in &message.blocks {
            match &block.kind {
                BlockKind::Text {
                    text: block_text, ..
                } => text.push_str(block_text),
                BlockKind::Thinking { thinking, .. } => {
                    if !thinking.is_empty() {
                        reasoning.push(thinking.clone());
                    }
                }
                BlockKind::ToolUse { input } => {
                    let arguments = if !input.is_empty() && input != "{}" {
                        input.clone()
                    } else {
                        block
                            .raw
                            .get("input")
                            .map(|value| {
                                serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
                            })
                            .unwrap_or_else(|| "{}".to_string())
                    };
                    tool_calls.push(ToolCall {
                        id: block
                            .raw
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: block
                            .raw
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        arguments,
                    });
                }
                BlockKind::Other => {}
            }
        }
        let input = message.usage.input + message.usage.cache_creation + message.usage.cache_read;
        usage.input += input;
        usage.cached += message.usage.cache_read;
        usage.output += message.usage.output;
        usage.reasoning += message.usage.thinking;
        usage.total += input + message.usage.output;
    }

    let stored: Vec<Value> = turn.iter().map(Accumulator::to_stored_json).collect();
    let opaque =
        serde_json::value::to_raw_value(&Value::Array(stored)).map_err(|error| ProviderError {
            message: format!("messages: encode replay payload: {error}"),
        })?;
    Ok(TurnResult {
        text,
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning.join("\n"))
        },
        tool_calls,
        usage: Some(usage),
        opaque: Some(opaque),
        upstream: None,
    })
}

// --- SSE framing -----------------------------------------------------------

/// One dispatched SSE event. Messages dispatches a blank line even when it
/// carries no `event:` name or `data:` lines.
struct Event {
    event: String,
    data: String,
}

#[derive(Default)]
struct Framer {
    event: String,
    data: String,
}

impl Framer {
    /// Feed one line. Returns the dispatched event at a blank line.
    fn push_line(&mut self, line: &[u8]) -> Option<Event> {
        if line.is_empty() {
            return Some(Event {
                event: std::mem::take(&mut self.event),
                data: std::mem::take(&mut self.data),
            });
        }
        let text = String::from_utf8_lossy(line);
        let (name, value) = match text.split_once(':') {
            Some((name, value)) => (name, value.strip_prefix(' ').unwrap_or(value)),
            None => (text.as_ref(), ""),
        };
        match name {
            // A line beginning with `:` is a comment.
            "" => {}
            "event" => self.event = value.to_string(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            _ => {}
        }
        None
    }
}

fn handle_event(
    accumulator: &mut Accumulator,
    event: &Event,
    on_delta: &mut DeltaSink,
    url: &Url,
    status: StatusCode,
) -> Result<(), ProviderError> {
    match event.event.as_str() {
        "ping" => return Ok(()),
        "error" => {
            let body = event.data.trim_end();
            return Err(ProviderError {
                message: api_error("POST", url, status, body.as_bytes()),
            });
        }
        "message_start"
        | "message_delta"
        | "message_stop"
        | "content_block_start"
        | "content_block_delta"
        | "content_block_stop" => {}
        // An untyped or unknown event is dispatched and then ignored.
        _ => return Ok(()),
    }
    let data: Value = serde_json::from_str(&event.data).map_err(|error| ProviderError {
        message: error.to_string(),
    })?;
    accumulator.accumulate(&data, on_delta)
}

/// Read one SSE line from the pooled response, enforcing the 32 MiB cap.
async fn next_line(
    response: &mut PooledResponse,
    buffer: &mut Vec<u8>,
    eof: &mut bool,
    ctx: &RunContext,
) -> Result<Option<Vec<u8>>, ProviderError> {
    loop {
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            if position > SSE_MAX_LINE {
                return Err(line_too_long());
            }
            let mut line: Vec<u8> = buffer.drain(..=position).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
        if buffer.len() > SSE_MAX_LINE {
            return Err(line_too_long());
        }
        if *eof {
            if buffer.is_empty() {
                return Ok(None);
            }
            // A final line with no trailing newline is returned normally; it is
            // not dispatched because no blank line follows it.
            let mut line = std::mem::take(buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
        tokio::select! {
            chunk = response.chunk() => match chunk? {
                Some(bytes) => buffer.extend_from_slice(&bytes),
                None => *eof = true,
            },
            _ = ctx.cancel.cancelled() => {
                return Err(ProviderError { message: "context canceled".to_string() });
            }
            _ = wait_deadline(ctx.deadline) => {
                return Err(ProviderError { message: "context deadline exceeded".to_string() });
            }
        }
    }
}

fn line_too_long() -> ProviderError {
    ProviderError {
        message: "bufio.Scanner: token too long".to_string(),
    }
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

// --- Retry policy (Go SDK requestconfig) -----------------------------------

/// The result of one send attempt.
enum SendOutcome {
    /// A 2xx response ready to stream.
    Success(PooledResponse),
    /// The attempt failed; `retry_or_fail` decides whether to retry.
    Failed(AttemptFailure),
}

/// One failed attempt, already classified for the retry decision.
enum AttemptFailure {
    /// Cancellation or the run deadline: never retried.
    Context(&'static str),
    /// A transport/connection failure: retried (Go `res == nil`).
    Connection(String),
    /// A retryable HTTP failure whose response was closed unread.
    Retry { retry_after: Option<Duration> },
    /// A failure that ends the turn; its error text is already built.
    Failure(ProviderError),
}

/// Go SDK `shouldRetry`: explicit `x-should-retry` wins over the status.
fn should_retry(status: u16, headers: &HeaderMap) -> bool {
    if let Some(value) = headers
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
    {
        if value == "true" {
            return true;
        }
        if value == "false" {
            return false;
        }
    }
    status == 408 || status == 409 || status == 429 || status >= 500
}

/// Go SDK `parseRetryAfterHeader`: `Retry-After-Ms` first, then `Retry-After`
/// (fractional seconds or an RFC 1123 HTTP date).
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
    };
    if let Some(value) = value("retry-after-ms")
        && let Ok(milliseconds) = value.parse::<f64>()
    {
        return Some(Duration::from_secs_f64(milliseconds / 1000.0).max(Duration::ZERO));
    }
    let value = value("retry-after")?;
    if let Ok(seconds) = value.parse::<f64>() {
        return Some(Duration::from_secs_f64(seconds.max(0.0)));
    }
    let when = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let remaining = when.with_timezone(&chrono::Utc) - chrono::Utc::now();
    // A date in the past means "retry now", not "use the exponential default".
    Some(remaining.to_std().unwrap_or(Duration::ZERO))
}

/// Why the caller stopped the run, if it did (Go `callerCtx.Err()`).
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

/// Sleep before a retry, waking early when the run is cancelled or its
/// deadline passes.
async fn wait_for_backoff(ctx: &RunContext, delay: Duration) -> Result<(), ProviderError> {
    if let Some(reason) = context_ended(ctx) {
        return Err(ProviderError {
            message: reason.to_string(),
        });
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = ctx.cancel.cancelled() => Err(ProviderError {
            message: "context canceled".to_string(),
        }),
        _ = wait_deadline(ctx.deadline) => Err(ProviderError {
            message: "context deadline exceeded".to_string(),
        }),
    }
}

// --- Shared helpers --------------------------------------------------------

/// Read a complete (non-streaming) response body.
async fn read_body(response: &mut PooledResponse) -> Vec<u8> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        body.extend_from_slice(&chunk);
    }
    body
}

/// Format an HTTP failure the way the Go SDK's `apierror.Error` does.
fn api_error(method: &str, url: &Url, status: StatusCode, body: &[u8]) -> String {
    let mut message = format!(
        "{method} {:?}: {} {}",
        url.as_str(),
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    if !body.is_empty() {
        message.push(' ');
        message.push_str(&String::from_utf8_lossy(body));
    }
    message
}

/// Bound text embedded in an error message (Go `truncateRunes`).
fn truncate_runes(text: &str, max: usize) -> String {
    let mut runes = text.chars();
    let truncated: String = runes.by_ref().take(max).collect();
    if runes.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn retryable_statuses_match_the_sdk() {
        for status in [408, 409, 429, 500, 502, 503, 599] {
            assert!(
                should_retry(status, &HeaderMap::new()),
                "{status} must be retryable"
            );
        }
        for status in [200, 201, 400, 401, 403, 404, 405, 422] {
            assert!(
                !should_retry(status, &HeaderMap::new()),
                "{status} must not be retryable"
            );
        }
        // The explicit header wins over the status, in both directions.
        assert!(should_retry(400, &headers(&[("x-should-retry", "true")])));
        assert!(!should_retry(500, &headers(&[("x-should-retry", "false")])));
    }

    #[test]
    fn retry_after_headers_are_parsed_in_preference_order() {
        // Retry-After-Ms wins over Retry-After and is fractional milliseconds.
        let both = headers(&[("retry-after-ms", "250"), ("retry-after", "9")]);
        assert_eq!(parse_retry_after(&both), Some(Duration::from_millis(250)));

        // Retry-After accepts fractional seconds.
        assert_eq!(
            parse_retry_after(&headers(&[("retry-after", "1.5")])),
            Some(Duration::from_millis(1500))
        );

        // An HTTP date in the past means "retry now", not the exponential default.
        assert_eq!(
            parse_retry_after(&headers(&[(
                "retry-after",
                "Sun, 06 Nov 1994 08:49:37 GMT"
            )])),
            Some(Duration::ZERO)
        );

        // A future HTTP date is honored.
        let future = (chrono::Utc::now() + chrono::Duration::seconds(120))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let parsed = parse_retry_after(&headers(&[("retry-after", &future)])).unwrap();
        assert!(
            parsed >= Duration::from_secs(100) && parsed <= Duration::from_secs(130),
            "future HTTP date parsed as {parsed:?}"
        );

        // Negative numeric values clamp to zero; absent headers fall through.
        assert_eq!(
            parse_retry_after(&headers(&[("retry-after", "-2")])),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
        assert_eq!(parse_retry_after(&headers(&[("retry-after", "")])), None);
    }

    #[test]
    fn default_retry_delay_is_exponential_bounded_and_jittered() {
        // Go: 0.5s * 2^attempt, capped at 8s, minus up to a quarter as jitter.
        let first = default_retry_delay(0, None);
        assert!(
            first >= Duration::from_millis(375) && first <= Duration::from_millis(500),
            "first retry delay {first:?}"
        );
        let second = default_retry_delay(1, None);
        assert!(
            second >= Duration::from_millis(750) && second <= Duration::from_millis(1000),
            "second retry delay {second:?}"
        );
        let capped = default_retry_delay(20, None);
        assert!(
            capped >= Duration::from_secs(6) && capped <= Duration::from_secs(8),
            "capped retry delay {capped:?}"
        );
        // A server-provided delay is used verbatim.
        assert_eq!(
            default_retry_delay(0, Some(Duration::from_millis(7))),
            Duration::from_millis(7)
        );
    }
}
