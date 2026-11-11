//! Progress notifications for a single tool call.
//!
//! Task 6 only fixes the shape of the per-call sink so [`super::server::CallContext`]
//! compiles. Task 7 implements `emit` (event mapping, the 200-rune summary, and
//! drop accounting).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use serde_json::Value;
use tokio::sync::mpsc;

/// Per-call progress state. The token is the client's `_meta.progressToken`;
/// `out` is the server's serialized-frame channel and `dropped` counts
/// notifications the bounded channel refused.
#[allow(dead_code)] // Task 7 implements the behavior over these fields.
pub struct ProgressSink {
    token: Option<Value>,
    out: mpsc::Sender<String>,
    dropped: Arc<AtomicU64>,
}

impl ProgressSink {
    pub fn new(token: Option<Value>, out: mpsc::Sender<String>, dropped: Arc<AtomicU64>) -> Self {
        Self {
            token,
            out,
            dropped,
        }
    }
}
