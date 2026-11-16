//! Projection of Codex-shaped rollout lines to an OpenCode session import body.
//!
//! The recorder already funnels every record through
//! `RolloutRecorder::write_line(kind, payload)`; this module consumes those same
//! lines and keeps just enough state to build OpenCode's import JSON
//! (`{info, messages, location}`). It is pure: no I/O, and the clock is an
//! argument, so it is deterministic in tests.
//!
//! Provider replay payloads (encrypted reasoning, signed thinking) never appear
//! in the lines it consumes and are never produced here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The agent id written on every imported session and assistant message.
const AGENT: &str = "subagent-mcp";
/// The title prefix and the longest first-prompt line kept in it.
const TITLE_PREFIX: &str = "subagent-mcp: ";
const TITLE_PROMPT_CHARS: usize = 80;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `ses_` + 12 hex digits of the millisecond time + 14 alphanumerics. Ids sort
/// by time and, within one millisecond, by creation order.
pub fn new_session_id(now_ms: u64) -> String {
    new_id("ses", now_ms)
}

/// Same shape as [`new_session_id`] with the `msg_` prefix.
pub fn new_message_id(now_ms: u64) -> String {
    new_id("msg", now_ms)
}

fn new_id(prefix: &str, now_ms: u64) -> String {
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff;
    let random = Uuid::new_v4().simple().to_string();
    format!(
        "{prefix}_{:012x}{:04x}{}",
        now_ms & 0xffff_ffff_ffff,
        counter,
        &random[..10]
    )
}

#[derive(Clone, Copy, Default)]
struct Tokens {
    input: u64,
    output: u64,
    reasoning: u64,
    cache_read: u64,
}

impl Tokens {
    fn add(&mut self, other: Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cache_read += other.cache_read;
    }

    fn json(self) -> Value {
        json!({
            "input": self.input,
            "output": self.output,
            "reasoning": self.reasoning,
            "cache": {"read": self.cache_read, "write": 0},
        })
    }
}

#[derive(Clone)]
enum Part {
    Text(String),
    Reasoning { text: String, created: u64 },
    Tool(Tool),
}

#[derive(Clone)]
struct Tool {
    id: String,
    name: String,
    input: Value,
    created: u64,
    completed: Option<u64>,
    output: Option<String>,
    is_error: bool,
}

#[derive(Clone)]
enum Message {
    User {
        id: String,
        created: u64,
        text: String,
    },
    Assistant(Assistant),
}

#[derive(Clone)]
struct Assistant {
    id: String,
    created: u64,
    completed: Option<u64>,
    parts: Vec<Part>,
    finish: Option<&'static str>,
    tokens: Option<Tokens>,
    /// A tool result was recorded in this message: the next model turn opens a
    /// new assistant message.
    has_tool_output: bool,
}

/// The accumulated session transcript.
#[derive(Clone, Default)]
pub struct Transcript {
    cwd: String,
    provider: String,
    model: String,
    first_ms: Option<u64>,
    last_ms: u64,
    first_prompt: Option<String>,
    failed: bool,
    totals: Tokens,
    messages: Vec<Message>,
    /// Index of the assistant message still accepting parts.
    open: Option<usize>,
    /// call id -> (message index, part index)
    tools: HashMap<String, (usize, usize)>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Consume one Codex-shaped rollout line seen at `now_ms`.
    pub fn push_line_at(&mut self, kind: &str, payload: &Value, now_ms: u64) {
        self.first_ms.get_or_insert(now_ms);
        self.last_ms = self.last_ms.max(now_ms);
        match kind {
            "session_meta" => {
                self.cwd = str_of(payload, "cwd");
                self.provider = str_of(payload, "model_provider");
            }
            "turn_context" => {
                let model = str_of(payload, "model");
                if !model.is_empty() {
                    self.model = model;
                }
            }
            "response_item" => self.response_item(payload, now_ms),
            "event_msg" => self.event_msg(payload, now_ms),
            _ => {}
        }
    }

    /// Record the result flag of a tool call. An errored call whose output is
    /// already recorded turns into an `error` state on the next projection.
    pub fn mark_tool_error(&mut self, call_id: &str) {
        if let Some(&(m, p)) = self.tools.get(call_id)
            && let Some(Message::Assistant(a)) = self.messages.get_mut(m)
            && let Some(Part::Tool(tool)) = a.parts.get_mut(p)
        {
            tool.is_error = true;
        }
    }

    fn response_item(&mut self, payload: &Value, now_ms: u64) {
        match payload.get("type").and_then(Value::as_str) {
            Some("message") => {
                let text = content_text(payload);
                match payload.get("role").and_then(Value::as_str) {
                    Some("user") => self.user_message(text, now_ms),
                    Some("assistant") => {
                        let a = self.assistant(now_ms);
                        a.parts.push(Part::Text(text));
                    }
                    _ => {}
                }
            }
            Some("reasoning") => {
                let text = content_text(payload);
                if !text.is_empty() {
                    let a = self.assistant(now_ms);
                    a.parts.push(Part::Reasoning {
                        text,
                        created: now_ms,
                    });
                }
            }
            Some("function_call") => {
                let args = str_of(payload, "arguments");
                let input = match serde_json::from_str::<Value>(&args) {
                    Ok(v @ Value::Object(_)) => v,
                    _ => json!({"raw": args}),
                };
                self.tool_call(payload, input, now_ms);
            }
            Some("custom_tool_call") => {
                let input = json!({"patch": str_of(payload, "input")});
                self.tool_call(payload, input, now_ms);
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                let call_id = str_of(payload, "call_id");
                let output = str_of(payload, "output");
                if let Some(&(m, p)) = self.tools.get(&call_id)
                    && let Some(Message::Assistant(a)) = self.messages.get_mut(m)
                {
                    if let Some(Part::Tool(tool)) = a.parts.get_mut(p) {
                        tool.output = Some(output);
                        tool.completed = Some(now_ms);
                    }
                    a.has_tool_output = true;
                }
            }
            _ => {}
        }
    }

    fn event_msg(&mut self, payload: &Value, now_ms: u64) {
        match payload.get("type").and_then(Value::as_str) {
            Some("token_count") => {
                let Some(last) = payload.pointer("/info/last_token_usage") else {
                    return;
                };
                let cached = num(last, "cached_input_tokens");
                let tokens = Tokens {
                    input: num(last, "input_tokens").saturating_sub(cached),
                    output: num(last, "output_tokens"),
                    reasoning: num(last, "reasoning_output_tokens"),
                    cache_read: cached,
                };
                self.totals.add(tokens);
                let a = self.assistant(now_ms);
                a.tokens.get_or_insert_with(Tokens::default).add(tokens);
            }
            Some("task_complete") => self.close_open("stop", now_ms),
            Some("error") => {
                self.failed = true;
                self.close_open("error", now_ms);
            }
            _ => {}
        }
    }

    fn user_message(&mut self, text: String, now_ms: u64) {
        // A new run starts: the session's outcome is that of the last run.
        self.failed = false;
        self.close_open("stop", now_ms);
        if self.first_prompt.is_none() {
            self.first_prompt = Some(text.clone());
        }
        self.messages.push(Message::User {
            id: new_message_id(now_ms),
            created: now_ms,
            text,
        });
    }

    /// The assistant message accepting parts, opening a new one when none is
    /// open or the open one already received a tool result (a later model turn).
    fn assistant(&mut self, now_ms: u64) -> &mut Assistant {
        let reopen = match self.open {
            Some(i) => matches!(&self.messages[i], Message::Assistant(a) if a.has_tool_output),
            None => true,
        };
        if reopen {
            self.close_open("tool-calls", now_ms);
            self.messages.push(Message::Assistant(Assistant {
                id: new_message_id(now_ms),
                created: now_ms,
                completed: None,
                parts: Vec::new(),
                finish: None,
                tokens: None,
                has_tool_output: false,
            }));
            self.open = Some(self.messages.len() - 1);
        }
        let i = self.open.expect("an assistant message is open");
        match &mut self.messages[i] {
            Message::Assistant(a) => a,
            Message::User { .. } => unreachable!("open index always names an assistant message"),
        }
    }

    fn tool_call(&mut self, payload: &Value, input: Value, now_ms: u64) {
        let call_id = str_of(payload, "call_id");
        let name = str_of(payload, "name");
        let a = self.assistant(now_ms);
        a.parts.push(Part::Tool(Tool {
            id: call_id.clone(),
            name,
            input,
            created: now_ms,
            completed: None,
            output: None,
            is_error: false,
        }));
        let p = a.parts.len() - 1;
        let m = self.open.expect("assistant() opened a message");
        self.tools.insert(call_id, (m, p));
    }

    fn close_open(&mut self, finish: &'static str, now_ms: u64) {
        if let Some(i) = self.open.take()
            && let Message::Assistant(a) = &mut self.messages[i]
        {
            a.completed = Some(now_ms);
            // A message that issued tool calls ends in `tool-calls` unless the
            // run itself ended (`stop` / `error` from the caller).
            let has_tool = a.parts.iter().any(|p| matches!(p, Part::Tool(_)));
            a.finish = Some(if finish == "tool-calls" && !has_tool {
                "stop"
            } else {
                finish
            });
        }
    }

    /// The OpenCode import body for this transcript.
    pub fn to_import(&self, session_id: &str, project_id: &str) -> Value {
        let created = self.first_ms.unwrap_or(0);
        let model = json!({
            "id": if self.model.is_empty() { "unknown" } else { &self.model },
            "providerID": if self.provider.is_empty() { AGENT } else { &self.provider },
        });
        let first_line = self
            .first_prompt
            .as_deref()
            .and_then(|p| p.lines().next())
            .unwrap_or("");
        let title: String = format!(
            "{TITLE_PREFIX}{}",
            first_line
                .chars()
                .take(TITLE_PROMPT_CHARS)
                .collect::<String>()
        );
        let location = json!({"directory": self.cwd});
        let messages: Vec<Value> = self
            .messages
            .iter()
            .map(|m| message_json(m, &model))
            .collect();
        json!({
            "info": {
                "id": session_id,
                "projectID": project_id,
                "agent": AGENT,
                "model": model,
                "cost": 0,
                "tokens": self.totals.json(),
                "outcome": if self.failed { "failed" } else { "succeeded" },
                "time": {"created": created, "updated": self.last_ms.max(created)},
                "title": title,
                "location": location,
            },
            "messages": messages,
            "location": location,
        })
    }
}

fn message_json(message: &Message, model: &Value) -> Value {
    match message {
        Message::User { id, created, text } => json!({
            "id": id,
            "type": "user",
            "time": {"created": created},
            "text": text,
        }),
        Message::Assistant(a) => {
            let content: Vec<Value> = a.parts.iter().map(part_json).collect();
            let mut time = Map::new();
            time.insert("created".into(), json!(a.created));
            if let Some(done) = a.completed {
                time.insert("completed".into(), json!(done));
            }
            let mut out = Map::new();
            out.insert("id".into(), json!(a.id));
            out.insert("type".into(), json!("assistant"));
            out.insert("time".into(), Value::Object(time));
            out.insert("agent".into(), json!(AGENT));
            out.insert("model".into(), model.clone());
            out.insert("content".into(), Value::Array(content));
            out.insert("cost".into(), json!(0));
            if let Some(finish) = a.finish {
                out.insert("finish".into(), json!(finish));
            }
            if let Some(tokens) = a.tokens {
                out.insert("tokens".into(), tokens.json());
            }
            Value::Object(out)
        }
    }
}

fn part_json(part: &Part) -> Value {
    match part {
        Part::Text(text) => json!({"type": "text", "text": text}),
        Part::Reasoning { text, created } => json!({
            "type": "reasoning",
            "text": text,
            "time": {"created": created, "completed": created},
        }),
        Part::Tool(tool) => {
            let state = match (&tool.output, tool.is_error) {
                (None, _) => json!({"status": "running", "input": tool.input, "metadata": {}}),
                (Some(out), false) => json!({
                    "status": "completed",
                    "input": tool.input,
                    "content": [{"type": "text", "text": out}],
                }),
                (Some(out), true) => json!({
                    "status": "error",
                    "input": tool.input,
                    "error": {"type": "tool", "message": out},
                    "content": [{"type": "text", "text": out}],
                }),
            };
            let mut time = Map::new();
            time.insert("created".into(), json!(tool.created));
            if let Some(done) = tool.completed {
                time.insert("completed".into(), json!(done));
            }
            json!({
                "type": "tool",
                "id": tool.id,
                "name": tool.name,
                "executed": true,
                "time": Value::Object(time),
                "state": state,
            })
        }
    }
}

fn str_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn num(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// The concatenated `text` of a payload's `content` array.
fn content_text(payload: &Value) -> String {
    payload
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}
