//! Agent layer: system-prompt construction, the in-memory session manager,
//! and the rollout payload builders (Go `internal/agent`).

pub mod executor;
pub mod prompt;
pub mod rollout;
pub mod runner;
pub mod session;

pub use executor::{BuiltinToolExecutor, builtin_tools};
pub use prompt::{
    AGENTS_MD_MAX_BYTES, DEFAULT_SYSTEM_PROMPT, PromptError, load_agents_md, system_prompt,
};
pub use runner::{
    CANCELLED_TOOL_RESULT, CONTINUE_AFTER_EMPTY_PROMPT, CONTINUE_PROMPT, DELTA_FLUSH_BYTES,
    DELTA_FLUSH_INTERVAL, DeltaCoalescer, RunHooks, Runner, ToolExecutor, ToolResult,
    append_message, budget_error, message_size, rollout_writable_roots, truncate_messages,
};
pub use session::{
    AgentError, DEFAULT_MAX_TURNS, Emitter, Session, SessionError, SessionLease, SessionManager,
    SessionOptions, SessionState, effective_writable_roots,
};
