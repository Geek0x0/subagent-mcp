//! Server-initiated elicitation approvals.
//!
//! Mirrors Go `Server.Approve`: send an `elicitation/create` request with the
//! exact four-line message, wait at most five minutes, and treat a denial, a
//! cancellation, a protocol error, or a transport failure as `false`. Every
//! untrusted field is quoted with Go's `strconv.Quote` rules and truncated on a
//! UTF-8 boundary so model-controlled text cannot forge the prompt layout.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::mcp::jsonrpc::{Response, canonical_request_id};
use crate::util::approval::{ApprovalRequest, Approver};
use crate::util::json::go_quote;
use crate::util::run::RunContext;

/// The Go approval timeout: `context.WithTimeout(ctx, 5*time.Minute)`.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Go's `approvalFieldLimit`: the maximum number of bytes of one untrusted,
/// model-controlled field shown in the approval message.
const APPROVAL_FIELD_LIMIT: usize = 512;

/// Pending elicitation requests keyed by the server-assigned request ID.
pub struct ElicitationApprover {
    out: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    timeout: Duration,
    /// mcp-go allocates monotonic integer request IDs starting at 1 per
    /// connection.
    next_id: AtomicU64,
}

impl ElicitationApprover {
    pub fn new(out: mpsc::Sender<String>) -> Self {
        Self {
            out,
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout: APPROVAL_TIMEOUT,
            next_id: AtomicU64::new(0),
        }
    }

    /// Routes a client response to the pending elicitation it answers. Returns
    /// whether an entry matched. Only an `accept` action grants approval.
    pub async fn handle_response(&self, response: &Response) -> bool {
        let key = response
            .id
            .canonical
            .clone()
            .or_else(|| canonical_request_id(&response.id.raw))
            .unwrap_or_else(|| response.id.raw.to_string());
        let sender = self.pending.lock().unwrap().remove(&key);
        let Some(sender) = sender else {
            return false;
        };
        let accepted = response.error.is_none()
            && response
                .result
                .as_ref()
                .and_then(|result| result.get("action"))
                .and_then(Value::as_str)
                == Some("accept");
        let _ = sender.send(accepted);
        true
    }
}

/// Removes the pending entry even when the approval future is dropped (for
/// example by an outer timeout), so an abandoned elicitation cannot leak a map
/// entry or match a late response.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    key: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.key);
    }
}

#[async_trait::async_trait]
impl Approver for ElicitationApprover {
    async fn approve(&self, ctx: &RunContext, req: ApprovalRequest) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let raw_id = json!(id);
        // Key the pending map exactly as the response frame will be
        // canonicalized, so `1`, `1.0`, and `1e0` all match the same entry.
        let key = canonical_request_id(&raw_id).unwrap_or_else(|| raw_id.to_string());
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(key.clone(), sender);
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            key: key.clone(),
        };

        let frame = json!({
            "jsonrpc": "2.0",
            "id": raw_id,
            "method": "elicitation/create",
            "params": {
                "message": approval_message(&req),
                "requestedSchema": {"type": "object", "properties": {}},
            },
        });
        // Go's timeout wraps the whole request (send plus wait), not just the
        // response wait.
        let deadline = tokio::time::Instant::now() + self.timeout;
        tokio::select! {
            sent = self.out.send(frame.to_string()) => {
                if sent.is_err() {
                    return false;
                }
            }
            _ = ctx.cancel.cancelled() => return false,
            _ = tokio::time::sleep_until(deadline) => return false,
        }

        tokio::select! {
            decision = receiver => decision.unwrap_or(false),
            _ = ctx.cancel.cancelled() => false,
            _ = tokio::time::sleep_until(deadline) => false,
        }
    }
}

/// Runs one approval with a five-minute cap and parent-cancellation denial.
pub async fn request_approval(
    approver: &dyn Approver,
    ctx: &RunContext,
    req: ApprovalRequest,
) -> bool {
    let wait = async {
        tokio::select! {
            decision = approver.approve(ctx, req) => decision,
            _ = ctx.cancel.cancelled() => false,
        }
    };
    tokio::time::timeout(APPROVAL_TIMEOUT, wait)
        .await
        .unwrap_or(false)
}

fn approval_message(req: &ApprovalRequest) -> String {
    let target = req
        .command
        .as_deref()
        .filter(|command| !command.is_empty())
        .or(req.path.as_deref())
        .unwrap_or("");
    format!(
        "subagent-mcp approval request (thread {})\ntool: {}\ntarget: {}\nreason: {}",
        req.thread_id,
        sanitize_approval_field(&req.tool),
        sanitize_approval_field(target),
        sanitize_approval_field(&req.reason),
    )
}

/// Renders a model-controlled value (tool name, command or path, justification)
/// for the approval prompt as a single line.
///
/// `strconv.Quote` escapes newlines, carriage returns, control bytes, and ANSI
/// escape sequences, so no field content can forge a header line, fake the
/// layout, or hide the real payload below the fold, while a normal command or
/// path stays readable verbatim inside the quotes. Values longer than
/// [`APPROVAL_FIELD_LIMIT`] bytes are cut on a UTF-8 boundary and get a visible
/// marker stating the original length.
pub fn sanitize_approval_field(value: &str) -> String {
    let truncated = value.len() > APPROVAL_FIELD_LIMIT;
    let mut quoted = if truncated {
        let mut cut = APPROVAL_FIELD_LIMIT;
        while cut > 0 && !value.is_char_boundary(cut) {
            cut -= 1;
        }
        go_quote(&value[..cut])
    } else {
        go_quote(value)
    };
    if truncated {
        quoted.push_str(&format!(
            " … [truncated: original length {} bytes]",
            value.len()
        ));
    }
    quoted
}
