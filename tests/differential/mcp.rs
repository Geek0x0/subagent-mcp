//! MCP stdio differential scenarios.
//!
//! `differential_mcp_reference_*` exercises the harness against the pinned Go
//! binary alone, so the foundation is proven before the Rust serve lifecycle
//! exists. The comparison tests are ignored until Task 25B wires `cli::serve`:
//! the base commit's `serve` is a stub, and pretending otherwise would hide a
//! real failure.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::support::{McpProcess, normalize, reference_or_skip, rust_binary};

fn write_config(home: &Path) -> PathBuf {
    let path = home.join("config.toml");
    std::fs::write(
        &path,
        r#"
[providers.test]
api = "chat-completions"
base_url = "https://api.invalid"
env_key = "SUBAGENT_DIFFERENTIAL_UNSET_KEY"
default_model = "test-model"
models = [{ id = "test-model" }]
"#,
    )
    .unwrap();
    path
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "differential", "version": "1"}
        }
    })
}

fn run_scenario(binary: &Path, home: &Path, config: &Path, requests: &[Value]) -> Vec<Value> {
    let home_text = home.to_str().unwrap();
    let config_text = config.to_str().unwrap();
    let mut process = McpProcess::start(
        binary,
        &[],
        &[("HOME", home_text), ("SUBAGENT_MCP_CONFIG", config_text)],
    )
    .unwrap_or_else(|error| panic!("start {}: {error}", binary.display()))
    .with_timeout(Duration::from_secs(30));

    let responses = requests
        .iter()
        .map(|request| process.request(request.clone()))
        .collect();
    process.close();
    responses
}

#[test]
fn differential_mcp_reference_initialize_smoke() {
    let Some(reference) = reference_or_skip() else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());

    let responses = run_scenario(
        &reference.path(),
        home.path(),
        &config,
        &[initialize_request()],
    );
    assert_eq!(responses.len(), 1);
    let response = &responses[0];
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(response["result"]["serverInfo"]["name"], "subagent-mcp");
    // The reference is pinned, so this is the real baseline version rather
    // than a normalized placeholder.
    assert_eq!(response["result"]["serverInfo"]["version"], "0.13.1");
}

#[test]
fn differential_mcp_reference_tools_list_smoke() {
    let Some(reference) = reference_or_skip() else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());

    let responses = run_scenario(
        &reference.path(),
        home.path(),
        &config,
        &[
            initialize_request(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ],
    );
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, vec!["subagent", "subagent-reply"]);
}

#[test]
fn differential_mcp_reference_notifications_are_collected() {
    let Some(reference) = reference_or_skip() else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let config = write_config(home.path());
    let home_text = home.path().to_str().unwrap();
    let config_text = config.to_str().unwrap();
    let mut process = McpProcess::start(
        &reference.path(),
        &[],
        &[("HOME", home_text), ("SUBAGENT_MCP_CONFIG", config_text)],
    )
    .unwrap();

    let initialized = process.request(initialize_request());
    assert_eq!(initialized["id"], 1);
    process
        .notify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .unwrap();
    let pong = process.request(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    assert_eq!(pong["id"], 2);
    assert!(process.notifications().is_empty());
    assert!(process.take_notifications().is_empty());
    process.close();
}

fn assert_mcp_matches(requests: &[Value]) {
    let Some(reference) = reference_or_skip() else {
        return;
    };
    // Separate homes keep one binary's state from leaking into the other; the
    // temp-root normalizer makes the two paths compare equal.
    let go_home = tempfile::tempdir().unwrap();
    let rust_home = tempfile::tempdir().unwrap();
    let go_config = write_config(go_home.path());
    let rust_config = write_config(rust_home.path());

    let go = run_scenario(&reference.path(), go_home.path(), &go_config, requests);
    let rust = run_scenario(
        Path::new(rust_binary()),
        rust_home.path(),
        &rust_config,
        requests,
    );

    let go_ctx = normalize::NormalizeContext::new().with_temp_root(go_home.path());
    let rust_ctx = normalize::NormalizeContext::new().with_temp_root(rust_home.path());
    let go: Vec<Value> = go
        .into_iter()
        .map(|value| normalize::value(value, &go_ctx))
        .collect();
    let rust: Vec<Value> = rust
        .into_iter()
        .map(|value| normalize::value(value, &rust_ctx))
        .collect();

    assert_eq!(rust, go, "normalized MCP responses differ");
}

#[test]
fn differential_mcp_initialize_matches_reference() {
    assert_mcp_matches(&[initialize_request()]);
}

#[test]
fn differential_mcp_tools_list_matches_reference() {
    assert_mcp_matches(&[
        initialize_request(),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ]);
}

#[test]
fn differential_mcp_unknown_tool_matches_reference() {
    assert_mcp_matches(&[
        initialize_request(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "not-a-tool", "arguments": {}}
        }),
    ]);
}
