//! Task 12: Messages adapter credential-environment isolation.
//!
//! This is a **separate test binary** with a single test on purpose: it mutates
//! the process environment, and Rust 2024 makes `std::env::set_var` unsafe
//! because a concurrent `std::env::var` is undefined behavior. `wire_messages`
//! runs many tests in parallel and its adapter/transport read environment
//! variables (`env_key`, `SUBAGENT_MCP_DEBUG_STREAM`), so the mutation lives
//! here where no other test can observe it.

mod support;

use subagent_mcp::provider::ModelLister;
use subagent_mcp::provider::messages::Messages;
use support::{Mock, ScriptMessage, Scripted, request, run_turn, user_only};

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn apply(changes: &[(&'static str, &str)]) -> Self {
        let mut saved = Vec::new();
        for &(key, value) in changes {
            saved.push((key, std::env::var(key).ok()));
            // SAFETY: this binary contains exactly one test, so no other thread
            // reads the environment while these values are set; every saved
            // value is restored on drop.
            unsafe { std::env::set_var(key, value) };
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: this binary contains exactly one test, so no other thread
            // reads the environment; restore the pre-test value.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[tokio::test]
async fn environment_credentials_never_reach_the_wire() {
    const CONFIGURED_KEY: &str = "cfg-key-0123";
    const ENV_API_KEY: &str = "sk-ant-env-DIFFERENT-key";
    const ENV_AUTH_TOKEN: &str = "sk-ant-oat-SECRET-from-env";
    const ENV_GATEWAY: &str = "gw-secret";
    const ENV_OPENAI_KEY: &str = "sk-openai-env-DIFFERENT-key";

    let elsewhere = Mock::start(Vec::new()).await;
    let elsewhere_url = elsewhere.base.clone();
    let configured = Mock::start(vec![
        ScriptMessage::text("hello").scripted(),
        Scripted::json(br#"{"data":[{"id":"model-a"}],"has_more":false}"#.to_vec()),
    ])
    .await;

    let _guard = EnvGuard::apply(&[
        ("ANTHROPIC_API_KEY", ENV_API_KEY),
        ("ANTHROPIC_AUTH_TOKEN", ENV_AUTH_TOKEN),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            "X-Corp-Gateway-Token: gw-secret",
        ),
        ("ANTHROPIC_BASE_URL", &elsewhere_url),
        ("OPENAI_API_KEY", ENV_OPENAI_KEY),
        ("OPENAI_BASE_URL", &elsewhere_url),
    ]);

    let mut cfg = configured.config(1000);
    cfg.key = Some(CONFIGURED_KEY.to_string());

    let provider = Messages::new("anthropic", &cfg).unwrap();
    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    assert_eq!(result.expect("turn").text, "hello");
    let ids = provider
        .list_models()
        .await
        .expect("list models")
        .into_iter()
        .map(|info| info.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["model-a"]);

    assert_eq!(
        configured.request_count(),
        2,
        "want exactly a turn and a ListModels request"
    );
    assert_eq!(
        elsewhere.requests().len(),
        0,
        "no request may go to the environment base URL"
    );

    let forbidden_values = [
        ENV_API_KEY,
        ENV_AUTH_TOKEN,
        ENV_GATEWAY,
        ENV_OPENAI_KEY,
        elsewhere_url.as_str(),
    ];
    let forbidden_headers = [
        "authorization",
        "x-corp-gateway-token",
        "openai-organization",
    ];
    for index in 0..configured.request_count() {
        let recorded = configured.request(index);
        assert_eq!(
            recorded.header("x-api-key"),
            Some(CONFIGURED_KEY),
            "request {index} must carry only the configured key"
        );
        assert!(
            recorded.header("anthropic-version").is_some(),
            "request {index} must carry Anthropic-Version"
        );
        for (name, value) in &recorded.headers {
            assert!(
                !forbidden_headers.contains(&name.as_str()),
                "request {index} sent environment-derived header {name}: {value}"
            );
            for secret in forbidden_values {
                assert!(
                    !value.contains(secret),
                    "request {index} leaked {secret:?} into header {name}: {value}"
                );
            }
        }
    }
    assert!(
        configured
            .request(0)
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    );
}
