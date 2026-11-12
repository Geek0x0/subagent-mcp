//! Task 26: differential harness foundation.
//!
//! The harness compares the Rust binary against the pinned Go v0.13.1 release.
//! It only runs on Linux x86_64 because the reference artifact is
//! `subagent-mcp_0.13.1_linux_amd64.tar.gz`; every other target reports zero
//! differential tests instead of trying to execute a Linux binary.
//!
//! Fetch the reference once with `scripts/fetch-baseline.sh`. The support
//! module (`ReferenceBinary`, `McpProcess`, `normalize`) is the reusable
//! foundation for the Task 27 scenarios.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

#[path = "differential/support/mod.rs"]
mod support;

#[path = "differential/cli_config.rs"]
mod cli_config;

#[path = "differential/mcp.rs"]
mod mcp;

#[path = "differential/agent.rs"]
mod agent;

#[path = "differential/providers.rs"]
mod providers;

#[path = "differential/tools.rs"]
mod tools;

#[path = "differential/rollout.rs"]
mod rollout;
