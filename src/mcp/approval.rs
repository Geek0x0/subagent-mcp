//! Server-initiated elicitation approvals.
//!
//! Task 6 only fixes the shape so [`super::server::CallContext`] compiles and
//! the server can wire a per-connection approver. Task 7 implements the
//! elicitation request, the 5-minute timeout, and the `Approver` behavior.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::mcp::jsonrpc::Response;
use crate::util::approval::{ApprovalRequest, Approver};
use crate::util::run::RunContext;

/// The Go approval timeout: `context.WithTimeout(ctx, 5*time.Minute)`.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Pending elicitation requests keyed by the server-assigned request ID.
#[allow(dead_code)] // Task 7 implements the behavior over these fields.
pub struct ElicitationApprover {
    out: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    timeout: Duration,
}

impl ElicitationApprover {
    pub fn new(out: mpsc::Sender<String>) -> Self {
        Self {
            out,
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout: APPROVAL_TIMEOUT,
        }
    }

    /// Routes a client response to the pending elicitation it answers. Returns
    /// whether an entry matched. Task 7 implements the pending-map lookup.
    pub async fn handle_response(&self, _response: &Response) -> bool {
        false
    }
}

#[async_trait::async_trait]
impl Approver for ElicitationApprover {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        // Task 7 sends `elicitation/create` and waits for the client's action;
        // until then the server denies rather than granting silently.
        false
    }
}
