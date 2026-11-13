//! Auto model selection (`model: "auto"`).
//!
//! Ports Go `internal/server/auto.go` (v0.13.1): candidate filtering, the
//! one-candidate skip, the TypeSafe Jev decision request, and the metadata
//! object returned to the caller and written into the rollout. Missing decision
//! fields emit Go's zero values.
//!
//! Task 24B integrates Codex candidates: the adapter is registered by
//! [`super::tools::SubagentTools::new`], so [`codex_available`] is true and a
//! configured `codex-app-server` provider is offered to the decision model.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::agent::SessionManager;
use crate::config::Config;
use crate::decision::{Candidate, DecisionClient};
use crate::mcp::server::ToolError;
use crate::util::json::go_quote;
use crate::util::run::RunContext;

use super::tools::{api_key_for, codex_available, provider_names};

/// One resolved provider/model choice, with the decision metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoSelection {
    pub provider: String,
    pub model: String,
    pub skipped: bool,
    pub judge_model: Option<String>,
    pub confidence: Option<f64>,
    pub probabilities: BTreeMap<String, f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub omitted: Vec<String>,
}

impl AutoSelection {
    /// The `auto` result object, including Go's zero-value decision fields.
    pub fn meta(&self) -> Value {
        let mut meta = Map::new();
        if self.skipped {
            return json!({"skipped": true});
        } else {
            meta.insert(
                "judge_model".into(),
                json!(self.judge_model.as_deref().unwrap_or_default()),
            );
            meta.insert("confidence".into(), json!(self.confidence.unwrap_or(0.0)));
            if !self.probabilities.is_empty() {
                meta.insert("probabilities".into(), json!(self.probabilities));
            }
            meta.insert("input_tokens".into(), json!(self.input_tokens.unwrap_or(0)));
            meta.insert(
                "output_tokens".into(),
                json!(self.output_tokens.unwrap_or(0)),
            );
        }
        if !self.omitted.is_empty() {
            meta.insert("omitted".into(), json!(self.omitted));
        }
        Value::Object(meta)
    }
}

/// Choose a configured provider/model with the decision model and return its
/// metadata. A failed decision is an error, never a fallback.
///
/// `manager` is accepted for interface parity with the Go server; selection
/// itself never creates a session.
pub async fn resolve_auto(
    ctx: &RunContext,
    config: &Config,
    _manager: &SessionManager,
    task: &str,
    cwd: &Path,
) -> Result<AutoSelection, ToolError> {
    let auto = config.auto.as_ref().ok_or_else(|| {
        ToolError::internal(format!(
            "model \"auto\" requires an [auto] section in {}",
            config.path.display()
        ))
    })?;

    let (candidates, omitted) = auto_candidates(config);
    if candidates.is_empty() {
        return Err(ToolError::internal(format!(
            "auto model selection: no provider is usable: {}",
            omitted.join("; ")
        )));
    }

    let (provider, model, decision) = if candidates.len() == 1 {
        (
            candidates[0].provider.clone(),
            candidates[0].model.clone(),
            None,
        )
    } else {
        let key = auto_api_key(config)?;
        let mut auto_cfg = auto.clone();
        auto_cfg.key = Some(key);
        let client = DecisionClient::new(&auto_cfg)
            .map_err(|e| ToolError::internal(format!("auto model selection: {e}")))?;
        let decision = client
            .choose(ctx, task, cwd, &candidates)
            .await
            .map_err(|e| ToolError::internal(format!("auto model selection: {e}")))?;
        (
            decision.provider.clone(),
            decision.model.clone(),
            Some(decision),
        )
    };

    let provider_cfg = config.providers.get(&provider).ok_or_else(|| {
        ToolError::internal(format!(
            "auto model selection: unknown provider {}; configured providers: {}",
            go_quote(&provider),
            provider_names(config).join(", ")
        ))
    })?;
    if !has_model(provider_cfg, &model) {
        return Err(ToolError::internal(format!(
            "auto model selection: chose unknown model {} for provider {}",
            go_quote(&model),
            go_quote(&provider)
        )));
    }
    // Resolve the key now so a missing credential fails with the same
    // `auto model selection:` prefix the Go server uses.
    api_key_for(config, &provider)
        .map_err(|e| ToolError::internal(format!("auto model selection: {e}")))?;

    let skipped = candidates.len() == 1;
    Ok(match decision {
        None => AutoSelection {
            provider,
            model,
            skipped: true,
            judge_model: None,
            confidence: None,
            probabilities: BTreeMap::new(),
            input_tokens: None,
            output_tokens: None,
            omitted,
        },
        Some(decision) => AutoSelection {
            provider,
            model,
            skipped,
            judge_model: decision.judge_model,
            confidence: Some(decision.confidence),
            probabilities: decision.probabilities,
            input_tokens: decision.input_tokens,
            output_tokens: decision.output_tokens,
            omitted,
        },
    })
}

/// List the models of every provider that can start a session. HTTP providers
/// without their key are omitted with all their models; Codex providers
/// authenticate out of band, so they are included whenever the adapter is
/// registered in this build.
fn auto_candidates(config: &Config) -> (Vec<Candidate>, Vec<String>) {
    let mut candidates = Vec::new();
    let mut omitted = Vec::new();
    for name in provider_names(config) {
        let provider = &config.providers[&name];
        if provider.api == crate::config::Api::CodexAppServer {
            if !codex_available() {
                omitted.push(format!(
                    "{name}: codex-app-server adapter is not available in this build"
                ));
                continue;
            }
        } else if let Err(error) = api_key_for(config, &name) {
            omitted.push(format!("{name}: {error}"));
            continue;
        }
        for model in &provider.models {
            candidates.push(Candidate {
                provider: name.clone(),
                model: model.id.clone(),
                api: provider.api.as_str().to_string(),
                description: model_description(&model.id, &model.description),
            });
        }
    }
    (candidates, omitted)
}

fn has_model(provider: &crate::config::ProviderConfig, id: &str) -> bool {
    provider.models.iter().any(|model| model.id == id)
}

/// The auto-selection API key (`auto.key`, else `auto.env_key`).
fn auto_api_key(config: &Config) -> Result<String, ToolError> {
    let auto = config
        .auto
        .as_ref()
        .ok_or_else(|| ToolError::internal("auto model selection is not configured"))?;
    if let Some(key) = auto.key.as_deref().filter(|key| !key.is_empty()) {
        return Ok(key.to_string());
    }
    let name = auto.env_key.as_deref().unwrap_or("");
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(ToolError::internal(format!(
            "auto model selection: environment variable {name} (auto.env_key) must be set to the decision model API key"
        ))),
    }
}

/// Go `decision.Description`: the configured description, a known default, a
/// default for the suffix of a prefixed model id, or empty.
fn model_description(model: &str, configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    if let Some(description) = default_description(model) {
        return description.to_string();
    }
    if let Some((_, suffix)) = model.rsplit_once('/')
        && let Some(description) = default_description(suffix)
    {
        return description.to_string();
    }
    String::new()
}

fn default_description(model: &str) -> Option<&'static str> {
    Some(match model {
        "claude-opus-5" | "claude-opus-5-5" => {
            "Anthropic flagship; strongest reasoning, long agentic and multi-file coding; slower, most expensive."
        }
        "claude-sonnet-5" | "claude-sonnet-5-5" => {
            "Anthropic balanced tier; strong coding and tool use; moderate speed and cost."
        }
        "claude-haiku-5-5" => {
            "Anthropic small model; simple edits, search, and summaries; fast and cheap."
        }
        "gpt-6-astra" => {
            "OpenAI top tier; hardest coding and long autonomous runs; slowest, most expensive."
        }
        "gpt-6.1-sol" => {
            "OpenAI everyday default; strong coding, research, and analysis; mid speed and cost."
        }
        "gpt-6.1-terra" => {
            "OpenAI cost tier; much faster and about half Sol's cost; somewhat weaker on hard coding."
        }
        "gpt-6-luna" => {
            "OpenAI lightweight model; quick questions, summaries, and simple edits; very cheap, not for complex code."
        }
        "deepseek-v4.1-flash" => {
            "DeepSeek agentic coding model; good for tool-heavy and long-context tasks; fast and very cheap."
        }
        "deepseek-v4-flash" => {
            "DeepSeek V4 fast tier; cheap, weaker than V4.1 Flash on agentic coding."
        }
        "gemini-3.6-flash" => {
            "Google Flash; solid coding and tool use without Pro cost; fast and cheap."
        }
        "glm-5.1" => "Z.ai GLM previous generation; coding-capable general model; mid cost.",
        "glm-5.3" => "Z.ai latest GLM; tuned for coding and strong at it; low cost.",
        "glm-5.3-flash" => "Z.ai GLM fast tier; suited to simple tasks; fast and cheap.",
        "kimi-k3" => {
            "Moonshot flagship thinking model; strongest Kimi for complex reasoning and agentic work; slower."
        }
        "kimi-k2.7-code" => "Moonshot coding-focused model; good for coding agents; moderate cost.",
        "kimi-k2.7-code-highspeed" => {
            "Moonshot K2.7 Code faster variant; good for coding agents; quicker responses."
        }
        "kimi-k2.6" => {
            "Moonshot previous flagship with open weights; solid coding; cheaper than K3."
        }
        "mimo-v2.6-pro" => "Xiaomi MiMo flagship; 1M context and decent coding; low cost.",
        "mimo-v2.6-flash" => {
            "Xiaomi MiMo fast tier with open weights; 1M context, weaker coding; very cheap."
        }
        _ => return None,
    })
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn absent_decision_fields_emit_go_zero_values() {
        let mut selection = AutoSelection {
            provider: "p".into(),
            model: "m".into(),
            skipped: false,
            judge_model: None,
            confidence: None,
            probabilities: BTreeMap::new(),
            input_tokens: None,
            output_tokens: None,
            omitted: vec![],
        };
        assert_eq!(
            selection.meta(),
            json!({"judge_model":"", "confidence":0.0, "input_tokens":0, "output_tokens":0})
        );
        selection.skipped = true;
        selection.omitted.push("unusable".into());
        assert_eq!(selection.meta(), json!({"skipped":true}));
    }
}
