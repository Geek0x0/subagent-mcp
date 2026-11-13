//! Auto-selection client for the TypeSafe Jev "System One" endpoint.
//!
//! Ports Go `internal/decision/client.go` (subagent-mcp v0.13.1): request and
//! response shapes, the choice-question payload, error wrapping, response-size
//! limiting, detail redaction, and the Cloudflare `result` envelope. The exact
//! subagent-mcp-authored error text is preserved; Go stdlib / SDK text (timeouts)
//! is only matched semantically.
//!
//! TASK 8 HOOK: `build_http_client` is the single construction site for the
//! reqwest client. It builds the client with `tls::client_config()` via
//! `use_preconfigured_tls`, so auto selection shares the platform TLS trust
//! policy with the provider layer. Keep all client construction here.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;

use crate::config::AutoConfig;
use crate::util::json::go_quote;
use crate::util::run::RunContext;

const MAX_CANDIDATES: usize = 255;
const MAX_RESPONSE_BYTES: usize = 1 << 20;
const MAX_ERROR_DETAIL_RUNES: usize = 512;

const MODEL_QUESTION: &str = "model";
const CHOICE_TYPE: &str = "choice";
const MODEL_INSTRUCTIONS: &str = "Select the configured provider and model best suited to complete the task in state. Weigh the task's required reasoning depth, coding and tool needs, and the speed/cost implied by each candidate's description. Choose exactly one candidate.";

// Defaults applied by Go `decision.New` (config::load applies them too).
const DEFAULT_AUTO_BASE_URL: &str = "https://api.typesafe.ai";
const DEFAULT_AUTO_TIMEOUT_SECONDS: u64 = 20;

// ---------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub api: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JevDecision {
    pub provider: String,
    pub model: String,
    pub judge_model: Option<String>,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct DecisionError {
    pub message: String,
}

impl std::fmt::Display for DecisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DecisionError {}

fn derr(msg: impl Into<String>) -> DecisionError {
    DecisionError {
        message: msg.into(),
    }
}

pub struct DecisionClient {
    http: reqwest::Client,
    cfg: AutoConfig,
}

impl DecisionClient {
    /// Build a client for `cfg`. An empty base URL or zero timeout takes the
    /// package default (matching Go `decision.New`). The Jev API key is resolved
    /// once from `cfg.key`, or else from the `cfg.env_key` environment variable
    /// (Go resolves it in `config.AutoAPIKey` before constructing the client),
    /// and stored in `cfg.key` because the struct has no separate key field.
    pub fn new(cfg: &AutoConfig) -> Result<Self, DecisionError> {
        let mut cfg = cfg.clone();
        if cfg.base_url.is_empty() {
            cfg.base_url = DEFAULT_AUTO_BASE_URL.to_string();
        }
        if cfg.timeout_seconds == 0 {
            cfg.timeout_seconds = DEFAULT_AUTO_TIMEOUT_SECONDS;
        }
        let key = resolve_api_key(&cfg)?;
        cfg.key = Some(key);
        let http = build_http_client(&cfg)?;
        Ok(Self { http, cfg })
    }

    /// Ask Jev to choose one of `candidates` for `task` in `cwd`. It fails
    /// rather than guesses when the answer is missing, not a choice, or names
    /// an unoffered candidate.
    pub async fn choose(
        &self,
        ctx: &RunContext,
        task: &str,
        cwd: &Path,
        candidates: &[Candidate],
    ) -> Result<JevDecision, DecisionError> {
        if candidates.is_empty() {
            return Err(derr("systemone: no candidates to choose from"));
        }
        if candidates.len() > MAX_CANDIDATES {
            return Err(derr(format!(
                "systemone: {} candidates exceed the maximum of {}",
                candidates.len(),
                MAX_CANDIDATES
            )));
        }

        let mut criteria = serde_json::Map::new();
        let mut choices: BTreeMap<String, &Candidate> = BTreeMap::new();
        for (i, cand) in candidates.iter().enumerate() {
            let key = format!("c{}", i + 1);
            criteria.insert(
                key.clone(),
                serde_json::Value::String(candidate_description(cand)),
            );
            choices.insert(key, cand);
        }

        let body_value = build_request_body(&self.cfg.model, task, cwd, criteria);
        let body_bytes = serde_json::to_vec(&body_value)
            .map_err(|e| derr(format!("systemone: encode request: {}", e)))?;

        let endpoint = if !self.cfg.url.is_empty() {
            self.cfg.url.clone()
        } else {
            format!("{}/v1/systemone", self.cfg.base_url.trim_end_matches('/'))
        };
        let api_key = self.cfg.key.as_deref().unwrap_or("");

        let req = self
            .http
            .post(endpoint.as_str())
            .bearer_auth(api_key)
            .header("Content-Type", "application/json")
            .header("TypeSafe-Danger-Zone-Long-Context", "true")
            .body(body_bytes)
            .build()
            .map_err(|e| derr(format!("systemone: build request: {}", e)))?;

        // The configured client timeout (reqwest `.timeout`), the run deadline,
        // and the run cancellation token all bound the whole request, matching
        // Go's `context.WithTimeout(ctx, c.timeout)` passed to the HTTP request.
        let (status, response_body) = {
            let operation = async {
                let response = self
                    .http
                    .execute(req)
                    .await
                    .map_err(|e| derr(format!("systemone: request: {}", e)))?;
                let status = response.status();
                let body = read_body_limited(response).await?;
                Ok::<(reqwest::StatusCode, Vec<u8>), DecisionError>((status, body))
            };
            let outcome: Result<(reqwest::StatusCode, Vec<u8>), DecisionError> = tokio::select! {
                result = operation => result,
                _ = ctx.cancel.cancelled() => {
                    Err(derr("systemone: request: context canceled"))
                }
                _ = deadline_wait(ctx.deadline) => {
                    Err(derr("systemone: request: context deadline exceeded"))
                }
            };
            outcome?
        };

        let code = status.as_u16();
        if !(200..300).contains(&code) {
            let mut detail = error_detail(&response_body, code);
            // Never echo the API key or the task back in the error text.
            for secret in [api_key, task] {
                if !secret.is_empty() {
                    detail = detail.replace(secret, "[redacted]");
                }
            }
            return Err(derr(format!(
                "systemone: status {}: {}",
                code,
                truncate(&detail, MAX_ERROR_DETAIL_RUNES)
            )));
        }

        let answer = decode_answer(&response_body)?;
        let choice = answer.answers.get(MODEL_QUESTION).ok_or_else(|| {
            derr(format!(
                "systemone: model {} returned no answer for question {}",
                go_quote(&answer.model),
                go_quote(MODEL_QUESTION)
            ))
        })?;
        if choice.kind != CHOICE_TYPE {
            return Err(derr(format!(
                "systemone: model {} answer type {} is not {}",
                go_quote(&answer.model),
                go_quote(&choice.kind),
                go_quote(CHOICE_TYPE)
            )));
        }
        let candidate = choices.get(&choice.choice).ok_or_else(|| {
            derr(format!(
                "systemone: model {} chose unknown choice {}",
                go_quote(&answer.model),
                go_quote(&choice.choice)
            ))
        })?;

        let mut probabilities = BTreeMap::new();
        for (id, probability) in &choice.probabilities {
            if let Some(offered) = choices.get(id) {
                probabilities.insert(candidate_key(offered), *probability);
            }
        }

        // Absent usage stays absent (Go used ints defaulting to 0; the Rust
        // interface keeps the distinction with Option).
        let (input_tokens, output_tokens) = match &answer.usage {
            Some(u) => (u.input_tokens, u.output_tokens),
            None => (None, None),
        };

        Ok(JevDecision {
            provider: candidate.provider.clone(),
            model: candidate.model.clone(),
            judge_model: opt_str(&answer.model),
            confidence: choice.confidence,
            probabilities,
            input_tokens,
            output_tokens,
        })
    }
}

// ---------------------------------------------------------------------------
// Client construction
// ---------------------------------------------------------------------------

/// The single construction site for the reqwest client. The TLS configuration
/// comes from [`crate::provider::tls::client_config`], so auto selection uses
/// the same platform trust policy as the provider transport.
fn build_http_client(cfg: &AutoConfig) -> Result<reqwest::Client, DecisionError> {
    let tls = crate::provider::tls::client_config()
        .map_err(|e| derr(format!("build HTTP client: {}", e)))?;
    build_http_client_with_tls(cfg, tls)
}

/// Build the client with an explicit rustls configuration. Kept separate so the
/// trust tests can prove the preconfigured TLS settings reach reqwest.
fn build_http_client_with_tls(
    cfg: &AutoConfig,
    tls: rustls::ClientConfig,
) -> Result<reqwest::Client, DecisionError> {
    let mut builder =
        crate::provider::proxy::configure(reqwest::Client::builder().use_preconfigured_tls(tls));
    if cfg.timeout_seconds > 0 {
        builder = builder.timeout(Duration::from_secs(cfg.timeout_seconds));
    }
    builder
        .build()
        .map_err(|e| derr(format!("build HTTP client: {}", e)))
}

fn resolve_api_key(cfg: &AutoConfig) -> Result<String, DecisionError> {
    if let Some(k) = cfg.key.as_deref().filter(|s| !s.is_empty()) {
        return Ok(k.to_string());
    }
    let name = cfg.env_key.as_deref().unwrap_or("");
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(derr(format!(
            "environment variable {} (auto.env_key) must be set to the decision model API key",
            name
        ))),
    }
}

async fn deadline_wait(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

fn build_request_body(
    model: &str,
    task: &str,
    cwd: &Path,
    criteria: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let mut state = serde_json::Map::new();
    state.insert(
        "task".to_string(),
        serde_json::Value::String(task.to_string()),
    );
    state.insert(
        "cwd".to_string(),
        serde_json::Value::String(cwd.to_string_lossy().into_owned()),
    );

    let mut question = serde_json::Map::new();
    question.insert(
        "type".to_string(),
        serde_json::Value::String(CHOICE_TYPE.to_string()),
    );
    question.insert(
        "instructions".to_string(),
        serde_json::Value::String(MODEL_INSTRUCTIONS.to_string()),
    );
    question.insert("criteria".to_string(), serde_json::Value::Object(criteria));

    let mut questions = serde_json::Map::new();
    questions.insert(
        MODEL_QUESTION.to_string(),
        serde_json::Value::Object(question),
    );

    let mut body = serde_json::Map::new();
    body.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    body.insert("state".to_string(), serde_json::Value::Object(state));
    body.insert(
        "questions".to_string(),
        serde_json::Value::Object(questions),
    );
    serde_json::Value::Object(body)
}

/// Go `json.Unmarshal` treats an explicit `null` for a non-pointer struct field
/// as "no effect" (the zero value: nil map, `""`, `0`) and produces no error.
/// serde's `#[serde(default)]` only covers a *missing* field, so route these
/// fields through a helper that collapses `null` into `T::default()` while
/// leaving real values intact; `#[serde(default)]` still handles the absent case.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct SystemOneResponse {
    #[serde(default, deserialize_with = "null_default")]
    model: String,
    #[serde(default, deserialize_with = "null_default")]
    answers: BTreeMap<String, ChoiceAnswer>,
    usage: Option<SystemOneUsage>,
}

#[derive(Deserialize)]
struct ChoiceAnswer {
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    kind: String,
    #[serde(default, deserialize_with = "null_default")]
    choice: String,
    #[serde(default, deserialize_with = "null_default")]
    confidence: f64,
    #[serde(default, deserialize_with = "null_default")]
    probabilities: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct SystemOneUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

/// Read the body with a `MAX_RESPONSE_BYTES` cap, matching Go's
/// `io.LimitReader(resp.Body, maxResponseBytes+1)`.
async fn read_body_limited(response: reqwest::Response) -> Result<Vec<u8>, DecisionError> {
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| derr(format!("systemone: read response: {}", e)))?;
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(derr(format!(
                "systemone: response body too large (limit {} bytes)",
                MAX_RESPONSE_BYTES
            )));
        }
    }
    Ok(buf)
}

/// Decode the answer, honoring Cloudflare's `{"result": ...}` envelope: when
/// `answers` is absent and `result` is present and not null, parse `result`.
fn decode_answer(body: &[u8]) -> Result<SystemOneResponse, DecisionError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| derr(format!("systemone: decode response: {}", e)))?;
    let answers_absent = value.get("answers").is_none();
    let use_result = answers_absent && value.get("result").is_some_and(|r| !r.is_null());
    let parsed: Result<SystemOneResponse, serde_json::Error> = if use_result {
        serde_json::from_value(value.get("result").cloned().unwrap())
    } else {
        serde_json::from_value(value)
    };
    parsed.map_err(|e| derr(format!("systemone: decode response: {}", e)))
}

// ---------------------------------------------------------------------------
// Error detail / helpers
// ---------------------------------------------------------------------------

/// Render the response's `detail` field (or joined `errors[].message`, or the
/// status text) and never the request, matching Go `errorDetail`.
fn error_detail(body: &[u8], status: u16) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return status_text(status);
    };
    let Some(obj) = value.as_object() else {
        return status_text(status);
    };
    match obj.get("detail") {
        None => {
            let mut messages = Vec::new();
            if let Some(errors) = obj.get("errors").and_then(|e| e.as_array()) {
                for item in errors {
                    if let Some(msg) = item.get("message").and_then(|m| m.as_str())
                        && !msg.is_empty()
                    {
                        messages.push(msg.to_string());
                    }
                }
            }
            if messages.is_empty() {
                status_text(status)
            } else {
                messages.join("; ")
            }
        }
        // Go tries to unmarshal `detail` into a string first.
        Some(serde_json::Value::String(s)) => s.clone(),
        // Otherwise json.Compact: serialize the value without whitespace.
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| v.to_string()),
    }
}

/// Go `http.StatusText` for the standard status codes.
fn status_text(status: u16) -> String {
    let text = match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Request Entity Too Large",
        418 => "I'm a teapot",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    };
    text.to_string()
}

/// Go `truncate`: cut to `limit` runes, no ellipsis.
fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn candidate_description(c: &Candidate) -> String {
    let mut s = format!("provider {} (api {}); model {}", c.provider, c.api, c.model);
    if !c.description.is_empty() {
        s.push_str("; ");
        s.push_str(&c.description);
    }
    s
}

fn candidate_key(c: &Candidate) -> String {
    format!("{}/{}", c.provider, c.model)
}

fn opt_str(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Inline unit tests (exact text of the internal helpers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(key: &str) -> AutoConfig {
        AutoConfig {
            base_url: String::new(),
            url: String::new(),
            env_key: None,
            key: Some(key.to_string()),
            model: "jev-latest".to_string(),
            timeout_seconds: 0,
        }
    }

    #[tokio::test]
    async fn new_applies_defaults_and_resolves_key() {
        let c = DecisionClient::new(&cfg("key")).unwrap();
        assert_eq!(c.cfg.base_url, DEFAULT_AUTO_BASE_URL);
        assert_eq!(c.cfg.timeout_seconds, DEFAULT_AUTO_TIMEOUT_SECONDS);
        assert_eq!(c.cfg.key.as_deref(), Some("key"));
    }

    #[tokio::test]
    async fn clients_retain_distinct_keys() {
        let a = DecisionClient::new(&cfg("first-key")).unwrap();
        let b = DecisionClient::new(&cfg("second-key")).unwrap();
        assert_eq!(a.cfg.key.as_deref(), Some("first-key"));
        assert_eq!(b.cfg.key.as_deref(), Some("second-key"));
    }

    #[test]
    fn candidate_description_matches_go() {
        let a = Candidate {
            provider: "alpha".into(),
            model: "m-fast".into(),
            api: "chat-completions".into(),
            description: "fast, cheap".into(),
        };
        assert_eq!(
            candidate_description(&a),
            "provider alpha (api chat-completions); model m-fast; fast, cheap"
        );
        let b = Candidate {
            provider: "bravo".into(),
            model: "org/model".into(),
            api: "codex-app-server".into(),
            description: String::new(),
        };
        assert_eq!(
            candidate_description(&b),
            "provider bravo (api codex-app-server); model org/model"
        );
    }

    #[test]
    fn candidate_key_matches_go() {
        let c = Candidate {
            provider: "bravo".into(),
            model: "org/model".into(),
            api: String::new(),
            description: String::new(),
        };
        assert_eq!(candidate_key(&c), "bravo/org/model");
    }

    #[test]
    fn error_detail_string_errors_compact_and_fallback() {
        assert_eq!(
            error_detail(br#"{"detail":"invalid API key"}"#, 401),
            "invalid API key"
        );
        assert_eq!(
            error_detail(
                br#"{"errors":[{"code":123,"message":"invalid model"},{"message":"boom"}]}"#,
                400
            ),
            "invalid model; boom"
        );
        assert_eq!(
            error_detail(
                br#"{"detail":{"loc":["body","state"],"msg":"state is too long"}}"#,
                422
            ),
            r#"{"loc":["body","state"],"msg":"state is too long"}"#
        );
        assert_eq!(error_detail(b"not json", 500), "Internal Server Error");
    }

    #[test]
    fn status_text_matches_go() {
        assert_eq!(status_text(400), "Bad Request");
        assert_eq!(status_text(401), "Unauthorized");
        assert_eq!(status_text(422), "Unprocessable Entity");
        assert_eq!(status_text(999), "");
    }

    #[test]
    fn truncate_counts_runes_without_ellipsis() {
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("éééé", 2), "éé");
        assert_eq!(truncate("short", 512), "short");
    }

    #[tokio::test]
    async fn build_http_client_uses_the_configured_tls_roots() {
        use crate::provider::tls;
        let cert = tls::test_support::ca_signed("127.0.0.1");
        let port = tls::test_support::serve(&cert);
        let config = tls::client_config_with_extra_roots(std::slice::from_ref(&cert.root))
            .expect("config with temporary root");
        let client = build_http_client_with_tls(&cfg("key"), config).unwrap();
        let response = client
            .get(format!("https://127.0.0.1:{port}/"))
            .send()
            .await
            .expect("temporary root rejected by reqwest");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn build_http_client_rejects_an_untrusted_certificate() {
        use crate::provider::tls;
        let cert = tls::test_support::ca_signed("localhost");
        let port = tls::test_support::serve(&cert);
        let client = build_http_client(&cfg("key")).unwrap();
        let err = client
            .get(format!("https://localhost:{port}/"))
            .send()
            .await
            .expect_err("untrusted certificate accepted");
        assert!(
            tls::test_support::mentions_certificate(&err),
            "expected a certificate rejection, got: {err}"
        );
    }
}
