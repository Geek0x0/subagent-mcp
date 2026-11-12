//! Agent layer: system-prompt construction, the in-memory session manager,
//! and the rollout payload builders (Go `internal/agent`).

pub mod prompt;
pub mod rollout;
pub mod session;

pub use prompt::{
    AGENTS_MD_MAX_BYTES, DEFAULT_SYSTEM_PROMPT, PromptError, load_agents_md, system_prompt,
};
pub use session::{
    AgentError, DEFAULT_MAX_TURNS, Emitter, Session, SessionError, SessionLease, SessionManager,
    SessionOptions, SessionState, effective_writable_roots,
};
