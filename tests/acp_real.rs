//! An ACP thread against a REAL agent on this machine.
//!
//! Ignored by default because it starts a real agent, which may need a login and
//! may spend tokens. Run it on purpose and say which agent to use:
//!
//! ```text
//! SUBAGENT_MCP_ACP_COMMAND=npx SUBAGENT_MCP_ACP_ARGS="-y <acp-agent-package>" \
//!     cargo test --test acp_real -- --ignored --nocapture
//! ```
//!
//! `SUBAGENT_MCP_ACP_ARGS` is split on whitespace. The test skips when
//! `SUBAGENT_MCP_ACP_COMMAND` is unset.

use std::sync::Arc;
use std::time::Duration;

use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::acp::thread::{self, Spawn};
use subagent_mcp::provider::{Thread, ThreadCallbacks, ThreadOptions};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;

/// Refuses everything: the smoke prompt needs no tools.
struct Deny;

#[async_trait::async_trait]
impl Approver for Deny {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        false
    }
}

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
#[ignore = "starts a real ACP agent; set SUBAGENT_MCP_ACP_COMMAND"]
async fn real_agent_echo() {
    let Some(command) = std::env::var_os("SUBAGENT_MCP_ACP_COMMAND") else {
        eprintln!("skipping: SUBAGENT_MCP_ACP_COMMAND is not set");
        return;
    };
    let args: Vec<String> = std::env::var("SUBAGENT_MCP_ACP_ARGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let cwd = tempfile::tempdir().unwrap();

    let mut spawn = Spawn::new("real", command.to_string_lossy().into_owned());
    spawn.args = args;
    let opts = ThreadOptions {
        model: std::env::var("SUBAGENT_MCP_ACP_MODEL").unwrap_or_else(|_| "default".into()),
        cwd: cwd.path().to_path_buf(),
        sandbox: Sandbox::ReadOnly,
        approval_policy: ApprovalPolicy::OnRequest,
        base_instructions: None,
        developer_instructions: None,
        writable_roots: Vec::new(),
        ephemeral: true,
    };

    let t = tokio::time::timeout(Duration::from_secs(120), thread::start(&ctx(), spawn, opts))
        .await
        .expect("start timed out")
        .expect("start failed");
    let callbacks = ThreadCallbacks {
        emit: Arc::new(|_kind, _ev| {}),
        approve: Arc::new(Deny),
    };
    let out = tokio::time::timeout(
        Duration::from_secs(180),
        t.run(
            ctx(),
            "Reply with the word ok and nothing else.".into(),
            None,
            callbacks,
        ),
    )
    .await
    .expect("turn timed out");
    let close = t.close().await;
    let out = out.expect("turn failed");
    eprintln!("agent replied: {out:?}");
    assert!(!out.trim().is_empty(), "empty reply");
    assert!(out.to_lowercase().contains("ok"), "reply lacks ok: {out:?}");
    close.expect("close failed");
}
