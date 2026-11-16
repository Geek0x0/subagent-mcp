//! The `opencode` child process gets a scrubbed environment and a neutral cwd.

#![cfg(unix)]

#[path = "support/fake_opencode.rs"]
mod fake_opencode;

use std::time::Duration;

use fake_opencode::Fake;
use subagent_mcp::rollout::opencode::api::ApiClient;

#[tokio::test]
async fn the_child_sees_no_server_secrets_and_runs_in_root() {
    // SAFETY: this binary has a single test, so nothing else touches the environment.
    unsafe { std::env::set_var("SUBAGENT_MCP_LEAK_PROBE", "leaked") };
    let fake = Fake::new().reply(0, "[]", "", 0);
    let client = ApiClient::new(fake.bin.clone(), Duration::from_secs(10));
    client.list_projects().await.unwrap();
    assert!(
        !fake.env_of(0).contains("SUBAGENT_MCP_LEAK_PROBE"),
        "{}",
        fake.env_of(0)
    );
    assert_eq!(fake.cwd_of(0), "/");
}
