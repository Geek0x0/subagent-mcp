//! Provider layer.
//!
//! [`tls`] holds the platform TLS verifier configuration shared by the provider
//! transport and the auto-selection decision client; [`types`] holds the
//! provider/thread/auth traits.
//!
//! The behavior modules are [`factory`] (adapter registry / `new` and
//! process-wide shutdown hooks), [`transport`] (shared `reqwest` clients,
//! header-only timeout, debug capture, upstream recorder, bounded
//! drain-on-drop, and JSON replay helpers), and [`sse`] (a byte/line reader
//! only; provider-specific framing state machines live in each adapter). The
//! adapters are [`chat_completions`], [`responses`], [`messages`], and the
//! [`codex`] client with its process pool.

pub mod chat_completions;
pub mod codex;
pub mod factory;
pub mod messages;
pub(crate) mod proxy;
pub mod responses;
pub mod sse;
pub mod tls;
pub mod transport;
pub mod types;

// Package-level shutdown surface, matching Go `provider.Shutdown()` /
// `provider.RegisterShutdown()` referenced by the EOF-cleanup design.
pub use factory::{register_shutdown, shutdown};

pub use chat_completions::ChatCompletions;
pub use messages::Messages;
pub use responses::Responses;

pub use types::{
    Agent, AuthChecker, AuthStatus, DeltaSink, Message, ModelInfo, ModelLister, Provider,
    ProviderError, Role, Thread, ThreadCallbacks, ThreadOptions, ToolCall, ToolSpec, TransportKind,
    TurnRequest, TurnResult, Usage,
};
