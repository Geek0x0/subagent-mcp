//! MCP stdio server layer.
//!
//! [`jsonrpc`] owns the wire frames and request-ID handling; later tasks build
//! the stdio server, worker pool, and tool dispatch on top of it.

pub mod jsonrpc;

pub use jsonrpc::{
    Frame, JsonRpcError, Notification, Request, RequestId, Response, RpcError, canonical_request_id,
};
