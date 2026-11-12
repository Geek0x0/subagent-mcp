//! Provider layer.
//!
//! Task 8 owns the platform TLS verifier configuration ([`tls`]) shared by the
//! provider transport and the auto-selection decision client. The Task 9
//! type/trait surface lives in [`types`], extracted early (without transport,
//! SSE, factory, or adapter behavior) because downstream tasks need the trait
//! objects before the Task 8 native-TLS gate clears.
//!
//! Task 9 adds the behavior modules: [`factory`] (adapter registry / `new`),
//! [`transport`] (shared `reqwest` clients, header-only timeout, debug capture,
//! upstream recorder, bounded drain-on-drop, and JSON replay helpers), and
//! [`sse`] (a byte/line reader only; provider-specific framing state machines
//! belong to the later adapter tasks).

pub mod factory;
pub mod sse;
pub mod tls;
pub mod transport;
pub mod types;

pub use types::{
    Agent, AuthChecker, AuthStatus, DeltaSink, Message, ModelInfo, ModelLister, Provider,
    ProviderError, Role, Thread, ThreadCallbacks, ThreadOptions, ToolCall, ToolSpec, TransportKind,
    TurnRequest, TurnResult, Usage,
};
