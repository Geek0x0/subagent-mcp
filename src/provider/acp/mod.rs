//! Agent Client Protocol provider (experimental).
//!
//! [`AcpAdapter`] runs any ACP agent over stdio, one child process per thread.
//! Like Codex, [`Provider::turn`] is unsupported because ACP runs whole threads.

pub mod events;
pub mod rpc;
pub mod thread;

use std::sync::{Arc, Once};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::config::ProviderConfig;
use crate::provider::{
    Agent, AuthChecker, AuthStatus, DeltaSink, ModelInfo, ModelLister, Provider, ProviderError,
    Thread, ThreadOptions, TurnRequest, TurnResult,
};
use crate::util::json::go_quote;
use crate::util::run::RunContext;

/// How long a capability probe (`initialize`) may take.
pub const CAPABILITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Delegates whole threads to a freshly spawned ACP agent.
pub struct AcpAdapter {
    name: String,
    cfg: ProviderConfig,
}

impl AcpAdapter {
    /// Build an adapter. Agents spawn lazily, one per thread.
    pub fn new(name: &str, cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        register_shutdown();
        if cfg.command.as_deref().unwrap_or_default().is_empty() {
            return Err(ProviderError {
                message: format!("acp provider {}: command is required", go_quote(name)),
            });
        }
        Ok(Self {
            name: name.to_string(),
            cfg: cfg.clone(),
        })
    }

    /// The resolved provider config.
    pub fn config(&self) -> &ProviderConfig {
        &self.cfg
    }

    fn spawn(&self) -> thread::Spawn {
        let mut spawn = thread::Spawn::new(
            self.name.clone(),
            self.cfg.command.clone().unwrap_or_default(),
        );
        spawn.args = self.cfg.args.clone();
        spawn.env = self.cfg.env.clone();
        spawn
    }

    fn capability_ctx() -> RunContext {
        RunContext {
            cancel: CancellationToken::new(),
            deadline: Some(tokio::time::Instant::now() + CAPABILITY_TIMEOUT),
        }
    }
}

#[async_trait]
impl Provider for AcpAdapter {
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
            message: format!(
                "acp provider {}: turn is not supported; acp runs whole threads",
                go_quote(&self.name)
            ),
        })
    }
}

#[async_trait]
impl Agent for AcpAdapter {
    async fn start_thread(
        &self,
        ctx: &RunContext,
        opts: ThreadOptions,
    ) -> Result<Box<dyn Thread>, ProviderError> {
        Ok(Box::new(thread::start(ctx, self.spawn(), opts).await?))
    }
}

#[async_trait]
impl ModelLister for AcpAdapter {
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self
            .cfg
            .models
            .iter()
            .map(|m| ModelInfo {
                id: m.id.clone(),
                description: m.description.clone(),
            })
            .collect())
    }
}

#[async_trait]
impl AuthChecker for AcpAdapter {
    async fn check_auth(&self) -> Result<AuthStatus, ProviderError> {
        thread::probe(&Self::capability_ctx(), self.spawn()).await
    }
}

/// Register the child-reaping shutdown hook once.
fn register_shutdown() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        crate::provider::factory::register_shutdown(Some(Arc::new(thread::kill_all)));
    });
}
