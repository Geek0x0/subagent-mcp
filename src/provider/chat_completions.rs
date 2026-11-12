//! Chat Completions adapter (Task 10).
//!
//! Ports Go `internal/provider/chatcompletions/{adapter,client}.go` onto the
//! Task 9 [`crate::provider::transport::PooledClient`] send path. Wire requests
//! are built with raw tool-schema bytes and opaque `reasoning_content` replay,
//! sent through `execute_json`/`execute` so `extra_body` and the 10-minute
//! response-header timeout are applied automatically, and read with a
//! line-based SSE framer matching go-openai v1.42.0 `stream_reader.go`: every
//! line matching `^data:\s*` is one event, blank and non-`data:` lines are not
//! events (subject to the 300-empty-message limit), an exact `[DONE]` ends the
//! stream, and there is no fixed scanner limit.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, HeaderMap, HeaderValue,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::config::ProviderConfig;
use crate::provider::transport::PooledClient;
use crate::provider::{
    DeltaSink, ModelInfo, ModelLister, Provider, ProviderError, Role, ToolCall, ToolSpec,
    TransportKind, TurnRequest, TurnResult, Usage,
};
use crate::util::run::RunContext;

// --- Constants (Go client.go / go-openai stream_reader.go) -----------------

/// Stream-setup retry budget (Go `openStreamWithRetry` `maxAttempts`).
pub const MAX_ATTEMPTS: usize = 4;
/// Consecutive non-`data:` lines tolerated before go-openai's
/// `ErrTooManyEmptyStreamMessages` (`defaultEmptyMessagesLimit`).
pub const EMPTY_MESSAGES_LIMIT: usize = 300;
const CHAT_PATH: &str = "/chat/completions";
const MODELS_PATH: &str = "/models";

/// A retry delay function `attempt -> Duration`; the seam tests override so a
/// failing stream setup does not really sleep (Go `Client.Backoff`).
pub type Backoff = Arc<dyn Fn(usize) -> Duration + Send + Sync>;

static BACKOFF_SEED: AtomicU64 = AtomicU64::new(0);

/// Go `defaultBackoff`: 1s, 2s, 4s with half the delay jittered away. A cheap
/// time+seed mix stands in for `math/rand/v2` (no `rand` dependency); every
/// draw stays within `[base/2, base]` and varies across draws.
pub fn default_backoff(attempt: usize) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.min(32));
    let half = base / 2;
    let span = half.as_nanos() as u64 + 1;
    let seed = BACKOFF_SEED.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
        ^ u64::from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos(),
        );
    half + Duration::from_nanos(seed % span)
}

/// Classification of a stream-setup failure (Go `isRetryable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupFailure {
    /// A response arrived with this HTTP status (0 = no usable status).
    HttpStatus(u16),
    /// The request never reached a server (Go `*url.Error` / `net.Error`).
    Connection,
    /// The caller cancelled the run.
    Canceled,
    /// The run deadline elapsed.
    DeadlineExceeded,
    /// Anything else (Go's "unrelated error").
    Other,
}

impl SetupFailure {
    /// Only 429/5xx (or a status-0/unusable response) and pre-server
    /// connection errors are transient; cancellation and deadlines never are.
    pub fn is_retryable(self) -> bool {
        match self {
            SetupFailure::HttpStatus(status) => status == 0 || status == 429 || status >= 500,
            SetupFailure::Connection => true,
            SetupFailure::Canceled | SetupFailure::DeadlineExceeded | SetupFailure::Other => false,
        }
    }
}

/// Free-function form of [`SetupFailure::is_retryable`] for callers/tests.
pub fn is_retryable(failure: SetupFailure) -> bool {
    failure.is_retryable()
}

// --- Wire request body (Go openai.ChatCompletionRequest) -------------------
//
// Tool `parameters` is a `Box<RawValue>` so the caller's schema bytes go out
// verbatim (numbers such as `1e0` are never re-encoded). Absent schemas become
// `{}` as the API requires an object, not `null`.

#[derive(Serialize, Debug)]
struct WireStreamOptions {
    include_usage: bool,
}

#[derive(Serialize, Debug)]
struct WireFunction {
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    parameters: Box<RawValue>,
}

#[derive(Serialize, Debug)]
struct WireTool {
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}

#[derive(Serialize, Debug, Default)]
struct WireFunctionCall {
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    arguments: String,
}

#[derive(Serialize, Debug)]
struct WireToolCall {
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunctionCall,
}

#[derive(Serialize, Debug, Default)]
struct WireMessage {
    role: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    content: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    reasoning_content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Debug)]
struct WireRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i64>,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    stream_options: WireStreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

// --- Streamed chunk body (Go openai.ChatCompletionStreamResponse) ----------

/// Go's `encoding/json` decodes an explicit `null` into the zero value of a
/// struct, string, slice, or integer field. serde rejects `null` for those
/// unless told otherwise, so every non-optional stream field uses this.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize, Debug, Default)]
struct WireCompletionDetails {
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_tokens: i64,
}

#[derive(Deserialize, Debug, Default)]
struct WirePromptDetails {
    #[serde(default, deserialize_with = "null_as_default")]
    cached_tokens: i64,
}

#[derive(Deserialize, Debug, Default)]
struct WireUsage {
    #[serde(default, deserialize_with = "null_as_default")]
    prompt_tokens: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    completion_tokens: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    total_tokens: i64,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<WireCompletionDetails>,
}

#[derive(Deserialize, Debug, Default)]
struct WireFunctionChunk {
    #[serde(default, deserialize_with = "null_as_default")]
    name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    arguments: String,
}

#[derive(Deserialize, Debug, Default)]
struct WireToolCallChunk {
    #[serde(default)]
    index: Option<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    function: WireFunctionChunk,
}

#[derive(Deserialize, Debug, Default)]
struct WireDelta {
    #[serde(default, deserialize_with = "null_as_default")]
    content: String,
    #[serde(default, deserialize_with = "null_as_default")]
    reasoning_content: String,
    #[serde(default, deserialize_with = "null_as_default")]
    tool_calls: Vec<WireToolCallChunk>,
}

#[derive(Deserialize, Debug, Default)]
struct WireChoice {
    #[serde(default, deserialize_with = "null_as_default")]
    delta: WireDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct StreamChunk {
    #[serde(default, deserialize_with = "null_as_default")]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

fn map_usage(usage: &WireUsage) -> Usage {
    Usage {
        input: usage.prompt_tokens,
        output: usage.completion_tokens,
        total: usage.total_tokens,
        cached: usage
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |details| details.cached_tokens),
        reasoning: usage
            .completion_tokens_details
            .as_ref()
            .map_or(0, |details| details.reasoning_tokens),
    }
}

// --- Opaque replay / tool schema validation --------------------------------

/// Extract `reasoning_content` from an opaque replay payload (Go `opaque`).
/// An absent or malformed payload simply replays no reasoning.
fn reasoning_from_opaque(opaque: Option<&RawValue>) -> String {
    let Some(opaque) = opaque else {
        return String::new();
    };
    serde_json::from_str::<serde_json::Value>(opaque.get())
        .ok()
        .and_then(|value| {
            value
                .get("reasoning_content")
                .and_then(|reasoning| reasoning.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Validate a tool schema (Go `provider.DecodeToolSchema`) and return the raw
/// bytes to put on the wire. Go's `json.RawMessage` is empty for an absent
/// schema; `Box<RawValue>`'s Default (`"null"`) is the Rust stand-in, and both
/// are sent as `{}`.
fn tool_parameters(spec: &ToolSpec) -> Result<Box<RawValue>, ProviderError> {
    let raw = spec.parameters.get();
    if raw.is_empty() || raw.trim() == "null" {
        return RawValue::from_string("{}".to_owned()).map_err(|error| ProviderError {
            message: format!("chat completions: tool {} schema: {error}", spec.name),
        });
    }
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|error| ProviderError {
        message: format!("chat completions: tool {} schema: {error}", spec.name),
    })?;
    if !value.is_object() {
        return Err(ProviderError {
            message: format!(
                "chat completions: tool {} schema: not a JSON object",
                spec.name
            ),
        });
    }
    Ok(spec.parameters.clone())
}

// --- Finish reasons (Go finishReasonError) --------------------------------

fn finish_reason_error(reason: &str) -> Result<(), ProviderError> {
    match reason {
        "stop" | "tool_calls" | "function_call" | "eos" => Ok(()),
        // Both "length" and "model_length" report the same Go-authored text.
        "length" | "model_length" => Err(ProviderError {
            message: r#"chat completions: finish_reason "length": response was truncated by the output token limit"#.to_string(),
        }),
        "content_filter" => Err(ProviderError {
            message: r#"chat completions: finish_reason "content_filter": response was stopped by the content filter"#.to_string(),
        }),
        "" => Err(ProviderError {
            message: "chat completions: stream ended without a finish reason (truncated response)"
                .to_string(),
        }),
        other => {
            // Go `log.Printf`: a compatible server's private vocabulary is
            // accepted, not treated as a failed turn.
            eprintln!(
                "chat completions: unrecognised finish_reason {other:?}; treating the turn as finished"
            );
            Ok(())
        }
    }
}

// --- Tool-call assembler (Go toolCallAssembler) ----------------------------

struct AssembledCall {
    call: ToolCall,
    index: i64,
}

struct ToolCallAssembler {
    calls: Vec<AssembledCall>,
    index_pos: HashMap<i64, usize>,
    last: i64,
}

impl ToolCallAssembler {
    fn new() -> Self {
        Self {
            calls: Vec::new(),
            index_pos: HashMap::new(),
            last: -1,
        }
    }

    fn add(&mut self, entry: &WireToolCallChunk) {
        let pos = self.position_for(entry);
        self.last = pos as i64;
        let call = &mut self.calls[pos].call;
        if !entry.id.is_empty() && call.id.is_empty() {
            call.id = entry.id.clone();
        }
        if !entry.function.name.is_empty() && call.name.is_empty() {
            call.name = entry.function.name.clone();
        }
        call.arguments.push_str(&entry.function.arguments);
    }

    fn position_for(&mut self, entry: &WireToolCallChunk) -> usize {
        if let Some(index) = entry.index {
            if let Some(&pos) = self.index_pos.get(&index) {
                return pos;
            }
            return self.append(index);
        }
        if self.last < 0 {
            return self.append(-1);
        }
        // No index: a new non-empty id the most recent call does not already
        // carry starts the next call; anything else continues that call.
        let last = self.last as usize;
        if !entry.id.is_empty()
            && !self.calls[last].call.id.is_empty()
            && self.calls[last].call.id != entry.id
        {
            return self.append(-1);
        }
        last
    }

    fn append(&mut self, index: i64) -> usize {
        let pos = self.calls.len();
        self.calls.push(AssembledCall {
            call: ToolCall::default(),
            index,
        });
        if index >= 0 {
            self.index_pos.insert(index, pos);
        }
        pos
    }

    fn result(self) -> Result<Vec<ToolCall>, ProviderError> {
        if self.calls.is_empty() {
            return Ok(Vec::new());
        }
        // Order by server index when every call carries one; otherwise keep
        // the order the server sent them.
        let all_indexed = self.calls.iter().all(|assembled| assembled.index >= 0);
        let mut order: Vec<usize> = (0..self.calls.len()).collect();
        if all_indexed {
            order.sort_by_key(|&pos| self.calls[pos].index);
        }
        let mut out = Vec::with_capacity(order.len());
        for pos in order {
            let call = &self.calls[pos].call;
            if call.id.is_empty() {
                return Err(ProviderError {
                    message: format!(
                        "chat completions: tool call {} has no id (name {:?})",
                        out.len(),
                        call.name
                    ),
                });
            }
            if call.name.is_empty() {
                return Err(ProviderError {
                    message: format!("chat completions: tool call {:?} has no name", call.id),
                });
            }
            out.push(call.clone());
        }
        Ok(out)
    }
}

// --- Setup retry loop ------------------------------------------------------

enum SetupError {
    Status { status: u16, message: String },
    Connection(String),
    Context(&'static str),
}

impl SetupError {
    fn failure(&self) -> SetupFailure {
        match self {
            SetupError::Status { status, .. } => SetupFailure::HttpStatus(*status),
            SetupError::Connection(_) => SetupFailure::Connection,
            SetupError::Context(reason) => {
                if *reason == "context deadline exceeded" {
                    SetupFailure::DeadlineExceeded
                } else {
                    SetupFailure::Canceled
                }
            }
        }
    }

    fn into_error(self) -> ProviderError {
        match self {
            SetupError::Status { status, message } => ProviderError {
                message: format!("chat completions: HTTP {status}: {message}"),
            },
            SetupError::Connection(message) => ProviderError { message },
            SetupError::Context(reason) => ProviderError {
                message: reason.to_string(),
            },
        }
    }
}

/// Go `isFailureStatusCode`: anything below 200 or at/above 400. A 3xx is not a
/// failure for the stream setup (the client follows redirects; a bodiless 3xx
/// then reads as an empty stream).
fn is_failure_status(status: u16) -> bool {
    !(200..400).contains(&status)
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

async fn wait_for_backoff(ctx: &RunContext, delay: Duration) -> Result<(), &'static str> {
    if let Some(reason) = context_ended(ctx) {
        return Err(reason);
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = ctx.cancel.cancelled() => Err("context canceled"),
        _ = wait_deadline(ctx.deadline) => Err("context deadline exceeded"),
    }
}

/// Drain a failed response's error body under the run context. Go reads the
/// body with `io.ReadAll` on a ctx-bound request, so cancellation or the
/// deadline aborts the read; without that bound an unterminated 429/5xx body
/// would hang the turn forever. Returns the context reason when the run ends
/// before the body does.
async fn drain_error_body(
    ctx: &RunContext,
    mut response: PooledResponse,
) -> Result<String, &'static str> {
    let mut buf = Vec::new();
    loop {
        tokio::select! {
            chunk = response.chunk() => match chunk {
                Ok(Some(bytes)) => buf.extend_from_slice(&bytes),
                // A read error ends the drain the way Go's `io.ReadAll` does;
                // the status classification still comes from the HTTP status.
                Ok(None) | Err(_) => break,
            },
            _ = ctx.cancel.cancelled() => return Err("context canceled"),
            _ = wait_deadline(ctx.deadline) => return Err("context deadline exceeded"),
        }
    }
    response.close().await;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// go-openai's `ReasoningValidator` (`reasoning_validator.go`): the SDK rejects
/// a `max_tokens` on an `o1*`/`o3*`/`o4*`/`gpt-5*` model before any request is
/// sent, with `ErrReasoningModelMaxTokensDeprecated` surfaced unwrapped. The
/// other validator checks (logprobs, temperature, top_p, n, penalties) cannot
/// fire here because this adapter never sets those fields.
fn reasoning_model_guard(model: &str, max_tokens: Option<i64>) -> Result<(), ProviderError> {
    let reasoning_model = ["o1", "o3", "o4", "gpt-5"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
    if reasoning_model && max_tokens.is_some_and(|tokens| tokens > 0) {
        return Err(ProviderError {
            message: "this model is not supported MaxTokens, please use MaxCompletionTokens"
                .to_string(),
        });
    }
    Ok(())
}

// Alias kept short for the retry helpers.
type PooledResponse = crate::provider::transport::PooledResponse;

// --- Chat-completions SSE framer (go-openai stream_reader.go) --------------

/// Trim exactly Go's regexp `\s` (`[\t\n\f\r ]`) after the `data:` prefix; the
/// whole line was already `trim()`-ed (Unicode whitespace), matching
/// `bytes.TrimSpace` in go-openai's `processLines`.
fn trim_sse_space(text: &str) -> &str {
    text.trim_start_matches([' ', '\t', '\n', '\r', '\x0c'])
}

/// Line-based framer: one event per `^data:\s*` line, no blank-line
/// requirement, no fixed line cap.
struct LineFramer {
    buf: Vec<u8>,
    eof: bool,
    empty_messages: usize,
}

impl LineFramer {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            eof: false,
            empty_messages: 0,
        }
    }

    /// The next `data:` payload, or `None` at `[DONE]`/EOF.
    async fn next_event(
        &mut self,
        response: &mut PooledResponse,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        loop {
            let Some(line) = self.next_line(response).await? else {
                return Ok(None);
            };
            let text = String::from_utf8_lossy(&line);
            let trimmed = text.trim();
            let Some(rest) = trimmed.strip_prefix("data:") else {
                // Blank / comment / non-data lines are not events. go-openai
                // counts them per event, so a data event resets the count.
                self.empty_messages += 1;
                if self.empty_messages > EMPTY_MESSAGES_LIMIT {
                    return Err(ProviderError {
                        message: "stream has sent too many empty messages".to_string(),
                    });
                }
                continue;
            };
            let payload = trim_sse_space(rest);

            // go-openai's errorPrefix: an event that is an API error object is
            // surfaced as `error, <message>` rather than parsed as a chunk.
            if payload.starts_with(r#"{"error":"#) {
                let message = serde_json::from_str::<serde_json::Value>(payload)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(|message| message.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| payload.to_string());
                return Err(ProviderError {
                    message: format!("error, {message}"),
                });
            }

            if payload == "[DONE]" {
                return Ok(None);
            }
            self.empty_messages = 0;
            return Ok(Some(payload.as_bytes().to_vec()));
        }
    }

    async fn next_line(
        &mut self,
        response: &mut PooledResponse,
    ) -> Result<Option<Vec<u8>>, ProviderError> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop(); // trailing '\n'
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            if self.eof {
                // go-openai's `ReadBytes('\n')` returns io.EOF for a final line
                // with no newline; that partial line is not delivered.
                self.buf.clear();
                return Ok(None);
            }
            match response.chunk().await? {
                Some(bytes) => self.buf.extend_from_slice(&bytes),
                None => self.eof = true,
            }
        }
    }
}

// --- Adapter ---------------------------------------------------------------

/// Chat Completions provider.
pub struct ChatCompletions {
    name: String,
    /// The Task 9 adapter send path: `execute_json`/`execute` apply
    /// `extra_body` and enforce the 10-minute response-header timeout.
    client: PooledClient,
    cfg: ProviderConfig,
    api_key: String,
    backoff: Backoff,
}

impl ChatCompletions {
    /// Build a chat-completions adapter, resolving the API key from `cfg.key`
    /// or, when that is empty, the environment variable named by `cfg.env_key`.
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
            client: PooledClient::from_config(TransportKind::ChatCompletions, cfg),
            cfg: cfg.clone(),
            api_key,
            backoff: Arc::new(default_backoff),
        })
    }

    /// Override the stream-setup retry delay (Go `Adapter.SetBackoff`).
    pub fn set_backoff(&mut self, backoff: Backoff) {
        self.backoff = backoff;
    }

    fn base(&self) -> &str {
        self.cfg.base_url.trim_end_matches('/')
    }

    fn auth_headers(&self, stream: bool) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|error| {
            ProviderError {
                message: format!("chat completions: authorization header: {error}"),
            }
        })?;
        headers.insert(AUTHORIZATION, auth);
        if stream {
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        } else {
            headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        Ok(headers)
    }

    fn build_body(&self, req: &TurnRequest) -> Result<Vec<u8>, ProviderError> {
        let mut messages = Vec::new();
        if !req.system.is_empty() {
            messages.push(WireMessage {
                role: "system".into(),
                content: req.system.clone(),
                ..Default::default()
            });
        }
        for message in &req.messages {
            match message.role {
                Role::User => messages.push(WireMessage {
                    role: "user".into(),
                    content: message.text.clone(),
                    ..Default::default()
                }),
                Role::Assistant => messages.push(WireMessage {
                    role: "assistant".into(),
                    content: message.text.clone(),
                    reasoning_content: reasoning_from_opaque(message.opaque.as_deref()),
                    tool_calls: message
                        .tool_calls
                        .iter()
                        .map(|call| WireToolCall {
                            id: call.id.clone(),
                            kind: "function".into(),
                            function: WireFunctionCall {
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        })
                        .collect(),
                    ..Default::default()
                }),
                Role::Tool => {
                    // Chat Completions has no tool-error flag; errors are
                    // carried in the content text (Go adapter.go).
                    messages.push(WireMessage {
                        role: "tool".into(),
                        content: message.text.clone(),
                        tool_call_id: message.tool_call_id.clone().filter(|id| !id.is_empty()),
                        ..Default::default()
                    });
                }
            }
        }
        let mut tools = Vec::with_capacity(req.tools.len());
        for spec in &req.tools {
            // Validate before any request can be sent; a malformed schema
            // must fail the turn here (Go TestTurnRejectsMalformedToolSchema).
            tools.push(WireTool {
                kind: "function".into(),
                function: WireFunction {
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    parameters: tool_parameters(spec)?,
                },
            });
        }
        let wire = WireRequest {
            model: req.model.clone(),
            messages,
            max_tokens: self.cfg.max_output_tokens.filter(|&tokens| tokens > 0),
            stream: true,
            tools,
            stream_options: WireStreamOptions {
                include_usage: true,
            },
            reasoning_effort: req.effort.clone().filter(|effort| !effort.is_empty()),
        };
        serde_json::to_vec(&wire).map_err(|error| ProviderError {
            message: format!("chat completions: encode request: {error}"),
        })
    }

    async fn open_stream_once(
        &self,
        ctx: &RunContext,
        url: &Url,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<PooledResponse, SetupError> {
        let response = tokio::select! {
            response = self.client.execute_json(url.clone(), headers.clone(), body) => {
                response.map_err(|error| SetupError::Connection(error.message))?
            }
            _ = ctx.cancel.cancelled() => return Err(SetupError::Context("context canceled")),
            _ = wait_deadline(ctx.deadline) => {
                return Err(SetupError::Context("context deadline exceeded"));
            }
        };
        let status = response.status().map_or(0, |status| status.as_u16());
        if is_failure_status(status) {
            let message = match drain_error_body(ctx, response).await {
                Ok(message) => message,
                Err(reason) => return Err(SetupError::Context(reason)),
            };
            return Err(SetupError::Status { status, message });
        }
        Ok(response)
    }

    async fn open_stream_with_retry(
        &self,
        ctx: &RunContext,
        url: &Url,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<PooledResponse, ProviderError> {
        for attempt in 0..MAX_ATTEMPTS {
            if let Some(reason) = context_ended(ctx) {
                return Err(ProviderError {
                    message: reason.to_string(),
                });
            }
            match self.open_stream_once(ctx, url, headers, body).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    let retryable = error.failure().is_retryable();
                    if attempt + 1 >= MAX_ATTEMPTS || !retryable {
                        return Err(error.into_error());
                    }
                    let delay = (self.backoff)(attempt);
                    if let Err(reason) = wait_for_backoff(ctx, delay).await {
                        return Err(ProviderError {
                            message: reason.to_string(),
                        });
                    }
                }
            }
        }
        unreachable!("the retry loop always returns from its last attempt")
    }

    async fn run_turn(
        &self,
        ctx: RunContext,
        req: TurnRequest,
        mut on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        let body = self.build_body(&req)?;
        // go-openai validates the request inside `CreateChatCompletionStream`
        // before it builds or sends anything, so this guard runs after the
        // tool-schema checks and before the retry loop (including its
        // context check): the SDK error, not a context error, wins.
        reasoning_model_guard(&req.model, self.cfg.max_output_tokens)?;
        let headers = self.auth_headers(true)?;
        let url =
            Url::parse(&format!("{}{CHAT_PATH}", self.base())).map_err(|error| ProviderError {
                message: format!("chat completions: request URL: {error}"),
            })?;
        let mut response = self
            .open_stream_with_retry(&ctx, &url, &headers, &body)
            .await?;

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut finish_reason = String::new();
        let mut assembler = ToolCallAssembler::new();
        let mut usage: Option<Usage> = None;
        let mut framer = LineFramer::new();
        let mut outcome: Result<(), ProviderError> = Ok(());

        loop {
            // Cancellation and the run deadline also stop a stream that is
            // waiting for its next event (Go's HTTP request is ctx-bound).
            let event = tokio::select! {
                event = framer.next_event(&mut response) => event,
                _ = ctx.cancel.cancelled() => Err(ProviderError {
                    message: "context canceled".to_string(),
                }),
                _ = wait_deadline(ctx.deadline) => Err(ProviderError {
                    message: "context deadline exceeded".to_string(),
                }),
            };
            let payload = match event {
                Ok(Some(payload)) => payload,
                Ok(None) => break,
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            };

            let chunk: StreamChunk = match serde_json::from_slice(&payload) {
                Ok(chunk) => chunk,
                Err(error) => {
                    outcome = Err(ProviderError {
                        message: format!("chat completions: decode stream chunk: {error}"),
                    });
                    break;
                }
            };
            if let Some(reported) = &chunk.usage {
                // Keep the last usage chunk the stream reports.
                usage = Some(map_usage(reported));
            }
            for choice in &chunk.choices {
                if let Some(reason) = &choice.finish_reason
                    && !reason.is_empty()
                {
                    finish_reason = reason.clone();
                }
                let delta = &choice.delta;
                if !delta.reasoning_content.is_empty() {
                    reasoning.push_str(&delta.reasoning_content);
                }
                if !delta.content.is_empty() {
                    content.push_str(&delta.content);
                    on_delta(delta.content.clone());
                }
                for entry in &delta.tool_calls {
                    assembler.add(entry);
                }
            }
        }

        let upstream = response.upstream();
        // Explicit bounded drain so a sentinel-but-still-open stream cannot
        // stall the turn (Go `defer stream.Close()`).
        response.close().await;
        outcome?;

        finish_reason_error(&finish_reason)?;
        let tool_calls = assembler.result()?;

        let opaque = if reasoning.trim().is_empty() {
            None
        } else {
            serde_json::value::to_raw_value(&serde_json::json!({
                "reasoning_content": reasoning,
            }))
            .ok()
        };
        let reasoning_out = if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        };

        Ok(TurnResult {
            text: content,
            reasoning: reasoning_out,
            tool_calls,
            usage,
            opaque,
            upstream,
        })
    }
}

#[async_trait::async_trait]
impl Provider for ChatCompletions {
    fn name(&self) -> &str {
        &self.name
    }

    async fn turn(
        &self,
        ctx: RunContext,
        req: TurnRequest,
        on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        self.run_turn(ctx, req, on_delta).await
    }
}

#[async_trait::async_trait]
impl ModelLister for ChatCompletions {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = Url::parse(&format!("{}{MODELS_PATH}", self.base())).map_err(|error| {
            ProviderError {
                message: format!("list models: {error}"),
            }
        })?;
        let headers = self.auth_headers(false)?;
        let mut request = reqwest::Request::new(reqwest::Method::GET, url);
        *request.headers_mut() = headers;
        let mut response = self
            .client
            .execute(request)
            .await
            .map_err(|error| ProviderError {
                message: format!("list models: {}", error.message),
            })?;
        let status = response.status().map_or(0, |status| status.as_u16());
        let mut buf = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            buf.extend_from_slice(&chunk);
        }
        response.close().await;
        if is_failure_status(status) {
            return Err(ProviderError {
                message: format!(
                    "list models: HTTP {status}: {}",
                    String::from_utf8_lossy(&buf)
                ),
            });
        }
        let value: serde_json::Value =
            serde_json::from_slice(&buf).map_err(|error| ProviderError {
                message: format!("list models: {error}"),
            })?;
        let mut models = Vec::new();
        if let Some(data) = value.get("data").and_then(|data| data.as_array()) {
            for model in data {
                if let Some(id) = model.get("id").and_then(|id| id.as_str()) {
                    models.push(ModelInfo {
                        id: id.to_string(),
                        description: String::new(),
                    });
                }
            }
        }
        Ok(models)
    }
}
