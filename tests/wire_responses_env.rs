//! Task 11 credential-environment test, in its own binary.
//!
//! `std::env::set_var` is unsafe because it may race another thread's
//! `env::var`. `PooledClient::send` reads `SUBAGENT_MCP_DEBUG_STREAM`, so this
//! test must not share a process with other tests that send requests. Keeping
//! it in a dedicated test binary (exactly one test) means every environment
//! read happens on this test's thread, after the variables are set and before
//! they are restored.

use subagent_mcp::provider::ModelLister;
use subagent_mcp::provider::responses::Responses;

#[path = "support/responses_fake.rs"]
mod fake;

use fake::*;

struct EnvRestore(Vec<(String, Option<String>)>);

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            // SAFETY: this binary contains only this test, so no other thread
            // reads the environment concurrently; every saved value is
            // restored here before the test ends.
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
async fn credentials_never_come_from_the_environment() {
    let hostile = [
        ("ANTHROPIC_API_KEY", "sk-ant-env-DIFFERENT-key"),
        ("ANTHROPIC_AUTH_TOKEN", "sk-ant-oat-SECRET-from-env"),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            "X-Corp-Gateway-Token: gw-secret",
        ),
        ("ANTHROPIC_BASE_URL", "http://127.0.0.1:1/elsewhere"),
        ("OPENAI_API_KEY", "sk-openai-env-DIFFERENT-key"),
        ("OPENAI_ADMIN_KEY", "sk-admin-env-SECRET"),
        ("OPENAI_ORG_ID", "org-real"),
        ("OPENAI_PROJECT_ID", "proj-real"),
        ("OPENAI_CUSTOM_HEADERS", "X-Corp-Gateway-Token: gw-SECRET"),
        ("OPENAI_BASE_URL", "http://127.0.0.1:1/elsewhere"),
    ];
    let restore = EnvRestore(
        hostile
            .iter()
            .map(|(key, _)| (key.to_string(), std::env::var(key).ok()))
            .collect(),
    );
    for (key, value) in hostile {
        // SAFETY: this binary contains only this test; see the module comment.
        unsafe { std::env::set_var(key, value) };
    }

    let fake = spawn_fake(vec![text_turn("hello")]).await;
    fake.set_models(&["model-a"]);
    let provider = Responses::new("openai", &provider_config(&fake.base, "cfg-key-0123")).unwrap();

    let result = run_turn(&provider, request("gpt-x")).await.unwrap();
    assert_eq!(result.text, "hello");
    let ids: Vec<String> = ModelLister::list_models(&provider)
        .await
        .unwrap()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(ids, vec!["model-a"]);
    assert_eq!(
        fake.request_count(),
        2,
        "the configured server must see exactly the turn and the model listing"
    );

    let secrets = [
        "sk-ant-env-DIFFERENT-key",
        "sk-ant-oat-SECRET-from-env",
        "gw-secret",
        "sk-openai-env-DIFFERENT-key",
        "sk-admin-env-SECRET",
        "org-real",
        "proj-real",
        "gw-SECRET",
    ];
    for index in 0..2 {
        let headers = fake.headers(index);
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer cfg-key-0123")
        );
        for forbidden in [
            "x-corp-gateway-token",
            "openai-organization",
            "openai-project",
        ] {
            assert!(
                !headers.contains_key(forbidden),
                "environment-derived header {forbidden} reached the wire"
            );
        }
        for value in headers.values() {
            for secret in secrets {
                assert!(
                    !value.contains(secret),
                    "environment value {secret} leaked into a header"
                );
            }
        }
    }

    drop(restore);
}
