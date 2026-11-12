//! Provider layer.
//!
//! Task 8 owns the platform TLS verifier configuration ([`tls`]) shared by the
//! provider transport and the auto-selection decision client. The Task 9
//! type/trait surface lives in [`types`], extracted early (without transport,
//! SSE, factory, or adapter behavior) because downstream tasks need the trait
//! objects before the Task 8 native-TLS gate clears. Task 9 will add the
//! behavior modules and consume these types.

pub mod tls;
pub mod types;

pub use types::{
    Agent, AuthChecker, AuthStatus, DeltaSink, Message, ModelInfo, ModelLister, Provider,
    ProviderError, Role, Thread, ThreadCallbacks, ThreadOptions, ToolCall, ToolSpec, TransportKind,
    TurnRequest, TurnResult, Usage,
};
