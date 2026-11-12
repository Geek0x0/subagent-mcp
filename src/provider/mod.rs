//! Provider layer.
//!
//! Task 8 owns the platform TLS verifier configuration ([`tls`]) shared by the
//! provider transport and the auto-selection decision client. Task 9 adds the
//! provider trait, the shared HTTP transport, and the SSE helpers.

pub mod tls;
pub mod types;

pub use types::{ToolCall, Usage};
