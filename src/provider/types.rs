//! Provider value types shared with the rollout recorder (Task 21 prerequisite).
//!
//! Task 9 owns the full provider layer (trait, transport, SSE helpers), but is
//! gated on native TLS evidence. These two plain value types are extracted early
//! so the Task 21 recorder can type its API; Task 9 will re-export/consume them
//! later. No TLS, transport, or provider behavior lives here.

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
