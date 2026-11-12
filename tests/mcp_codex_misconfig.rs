//! A `codex-app-server` provider whose adapter is not agent-capable must fail
//! clearly instead of silently running the HTTP turn loop.
//!
//! This lives in its own test binary because the provider factory slot is
//! process-wide and `tests/mcp_tools.rs` needs the real Codex adapter there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use subagent_mcp::agent::SessionManager;
use subagent_mcp::config::{Api, Config, ModelConfig, ProviderConfig};
use subagent_mcp::mcp::jsonrpc::RequestId;
use subagent_mcp::mcp::server::{CallContext, ToolHandler};
use subagent_mcp::mcp::tools::SubagentTools;
use subagent_mcp::provider::{
    DeltaSink, Provider, ProviderError, TurnRequest, TurnResult, factory,
};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;

struct PlainProvider;

#[async_trait]
impl Provider for PlainProvider {
    fn name(&self) -> &str {
        "plain"
    }

    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        Ok(TurnResult {
            text: "the HTTP loop must not run".into(),
            ..Default::default()
        })
    }
}

struct Allow;

#[async_trait]
impl Approver for Allow {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        true
    }
}

fn call_context() -> CallContext {
    CallContext {
        cancel: CancellationToken::new(),
        request_id: Some(RequestId::new(json!(1))),
        meta: None,
        progress: None,
        approver: Arc::new(Allow),
    }
}

fn config() -> Config {
    Config {
        path: PathBuf::from("/test/config.toml"),
        providers: BTreeMap::from([(
            "codex".to_string(),
            ProviderConfig {
                api: Api::CodexAppServer,
                base_url: String::new(),
                env_key: None,
                key: None,
                command: None,
                default_model: "gpt-6.1-terra".into(),
                models: vec![ModelConfig {
                    id: "gpt-6.1-terra".into(),
                    description: String::new(),
                }],
                effort_map: BTreeMap::new(),
                max_output_tokens: None,
                max_nudges: None,
                extra_body: None,
            },
        )]),
        auto: None,
    }
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or("")
}

#[tokio::test]
async fn codex_app_server_without_agent_capability_is_a_clear_error() {
    // Own the factory slot before `SubagentTools::new` registers the real
    // adapter: `register_if_absent` keeps this non-agent constructor.
    factory::register(
        Api::CodexAppServer.as_str(),
        Arc::new(|_name, _cfg, _key| Ok(Arc::new(PlainProvider) as Arc<dyn Provider>)),
    );

    let tools = SubagentTools::new(
        "subagent".into(),
        Arc::new(config()),
        Arc::new(SessionManager::new()),
    );
    let cwd = tempfile::tempdir().unwrap().keep();

    let result = tools
        .call(
            "subagent",
            json!({"prompt": "hello", "cwd": cwd}),
            call_context(),
        )
        .await
        .unwrap();
    assert_eq!(result["isError"], true, "result = {result}");
    assert!(result.get("structuredContent").is_none());
    let message = text(&result);
    assert!(message.contains("codex-app-server"), "{message}");
    assert!(message.contains("Agent capability"), "{message}");
    assert!(
        !message.contains("the HTTP loop must not run"),
        "a non-agent Codex provider must not run the HTTP loop: {message}"
    );

    // The failed start created no session, so it cannot be resumed.
    let reply = tools
        .call(
            "subagent-reply",
            json!({"threadId": "any-thread", "prompt": "retry"}),
            call_context(),
        )
        .await
        .unwrap();
    assert!(text(&reply).contains("unknown threadId"), "{reply}");
}
