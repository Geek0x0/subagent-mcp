//! Codex app-server adapter: JSON-RPC client and per-command child pool.
//!
//! Task 13A owns the transport half (Go `internal/provider/codexappserver`
//! `client.go`, `pool.go`, `process.go`, and the adapter skeleton). Thread
//! lifecycle, event mapping, approvals, model listing, and auth checking are
//! Task 13B. Until then [`CodexAdapter::turn`] reports the Go adapter's
//! unsupported-operation error.

pub mod client;
pub mod pool;

use std::sync::{Arc, Once};

use async_trait::async_trait;

use crate::config::ProviderConfig;
use crate::provider::{DeltaSink, Provider, ProviderError, TurnRequest, TurnResult};
use crate::util::run::RunContext;

/// Delegates whole threads to a shared Codex app-server child.
pub struct CodexAdapter {
    name: String,
    cfg: ProviderConfig,
    pool: Arc<pool::Pool>,
}

impl CodexAdapter {
    /// Build an adapter. The child starts lazily and uses Codex's own login.
    pub fn new(name: &str, cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        register_shutdown();
        Ok(Self {
            name: name.to_string(),
            cfg: cfg.clone(),
            pool: pool::Pool::for_command(cfg.command.as_deref().unwrap_or_default()),
        })
    }

    /// The configured provider name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The shared per-command pool this adapter uses.
    pub fn pool(&self) -> &Arc<pool::Pool> {
        &self.pool
    }

    /// The resolved provider config (Task 13B reads command/model settings).
    pub fn config(&self) -> &ProviderConfig {
        &self.cfg
    }
}

#[async_trait]
impl Provider for CodexAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        Err(ProviderError {
            message: "codex-app-server runs whole threads; Turn is not supported".to_string(),
        })
    }
}

/// Register the pool shutdown hook once, like Go's package `init`.
fn register_shutdown() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        crate::provider::factory::register_shutdown(Some(Arc::new(pool::shutdown)));
    });
}
