use crate::util::run::RunContext;

pub struct ApprovalRequest {
    pub tool: String,
    pub command: Option<String>,
    pub path: Option<String>,
    pub reason: String,
}

#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, ctx: &RunContext, req: ApprovalRequest) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    struct Always(bool);

    #[async_trait::async_trait]
    impl Approver for Always {
        async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
            self.0
        }
    }

    #[tokio::test]
    async fn approver_returns_its_decision() {
        let ctx = RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        };
        let req = ApprovalRequest {
            tool: "shell".into(),
            command: Some("ls".into()),
            path: None,
            reason: "needs access".into(),
        };
        assert!(Always(true).approve(&ctx, req).await);
        let req2 = ApprovalRequest {
            tool: "write_file".into(),
            command: None,
            path: Some("a.txt".into()),
            reason: "write".into(),
        };
        assert!(!Always(false).approve(&ctx, req2).await);
    }
}
