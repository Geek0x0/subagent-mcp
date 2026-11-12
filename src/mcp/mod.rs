//! MCP stdio server layer.
//!
//! [`jsonrpc`] owns the wire frames and request-ID handling; [`server`] owns
//! dispatch, the mcp-go-compatible worker pool, cancellation, and the protocol
//! surface; [`stdio`] owns the OS pipes; [`progress`] and [`approval`] hold the
//! per-call progress and elicitation state.

pub mod approval;
pub mod auto;
pub mod jsonrpc;
pub mod progress;
pub mod server;
pub mod stdio;
pub mod tools;

pub use auto::{AutoSelection, resolve_auto};
pub use jsonrpc::{
    Frame, JsonRpcError, Notification, Request, RequestId, Response, RpcError, canonical_request_id,
};
pub use server::{CallContext, McpError, McpServer, ToolDefinition, ToolError, ToolHandler};
pub use tools::{SubagentTools, codex_available};
