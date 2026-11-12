/// System-prompt construction, in-memory session management, and the agent
/// rollout payload builders (Task 22 / Go `internal/agent`).
pub mod agent;
pub mod cli;
pub mod config;
pub mod decision;
pub mod mcp;
pub mod patch;
pub mod policy;
/// Provider layer: shared value types/traits, platform TLS transport, SSE byte
/// reader, and the adapter registry/factory (Tasks 8–9).
pub mod provider;
pub mod repo;
pub mod rollout;
pub mod sandbox;
pub mod tools;
pub mod util;

pub const VERSION: &str = "0.14.0";
