//! Provider value types and trait surface (Task 9 interfaces).
//!
//! Task 9 owns the full provider layer, but is gated on the Task 8 native-TLS
//! evidence. This module holds the complete type/trait surface up front so
//! downstream tasks (rollout, session manager, runner) can type their APIs.
//! It is deliberately **behavior-free**: no TLS, no HTTP transport, no SSE,
//! no factory, and no adapter implementations live here. Task 9 will add
//! `factory.rs`, `transport.rs`, `sse.rs` and the adapters and consume these
//! types; nothing here performs a provider turn.

use std::path::PathBuf;
use std::sync::Arc;

use crate::policy::{ApprovalPolicy, Sandbox};
use crate::util::run::RunContext;

/// Streamed text deltas produced during one provider turn.
pub type DeltaSink = Box<dyn FnMut(String) + Send>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Default)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    pub is_error: bool,
    /// Opaque provider-specific replay payload, preserved as raw JSON bytes.
    pub opaque: Option<Box<serde_json::value::RawValue>>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Box<serde_json::value::RawValue>,
}

#[derive(Clone, Debug, Default)]
pub struct TurnRequest {
    pub model: String,
    pub effort: Option<String>,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Clone, Debug, Default)]
pub struct TurnResult {
    pub text: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    /// Opaque provider replay payload, preserved as raw JSON bytes.
    pub opaque: Option<Box<serde_json::value::RawValue>>,
    pub upstream: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: i64,
    pub cached: i64,
    pub output: i64,
    pub reasoning: i64,
    pub total: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderError {
    pub message: String,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub description: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportKind {
    #[default]
    ChatCompletions,
    Responses,
    Messages,
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync + std::any::Any {
    fn name(&self) -> &str;
    async fn turn(
        &self,
        ctx: RunContext,
        req: TurnRequest,
        on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError>;
}

#[async_trait::async_trait]
pub trait ModelLister {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
}

#[derive(Clone, Debug)]
pub struct ThreadOptions {
    pub model: String,
    pub cwd: PathBuf,
    pub sandbox: Sandbox,
    pub approval_policy: ApprovalPolicy,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    pub writable_roots: Vec<PathBuf>,
    pub ephemeral: bool,
}

#[derive(Clone)]
pub struct ThreadCallbacks {
    // The exact Task 9 interface; a type alias would diverge from the plan.
    #[allow(clippy::type_complexity)]
    pub emit: Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>,
    pub approve: Arc<dyn crate::util::approval::Approver>,
}

#[async_trait::async_trait]
pub trait Agent: Send + Sync {
    async fn start_thread(
        &self,
        ctx: &RunContext,
        opts: ThreadOptions,
    ) -> Result<Box<dyn Thread>, ProviderError>;
}

#[async_trait::async_trait]
pub trait Thread: Send + Sync {
    async fn run(
        &self,
        ctx: RunContext,
        prompt: String,
        effort: Option<String>,
        callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError>;
    async fn close(&self) -> Result<(), ProviderError>;
}

#[async_trait::async_trait]
pub trait AuthChecker: Send + Sync {
    async fn check_auth(&self) -> Result<AuthStatus, ProviderError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthStatus {
    pub summary: String,
    pub version: Option<String>,
}
