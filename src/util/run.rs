#[derive(Clone)]
pub struct RunContext {
    pub cancel: tokio_util::sync::CancellationToken,
    pub deadline: Option<tokio::time::Instant>,
}

impl RunContext {
    /// Derive a child context: a child cancellation token that is cancelled
    /// when the parent is cancelled (but never the other way around), plus the
    /// same absolute deadline.
    pub fn child(&self) -> Self {
        Self {
            cancel: self.cancel.child_token(),
            deadline: self.deadline,
        }
    }

    /// When the run's deadline has elapsed, return the reason signalling that
    /// the deadline — rather than an explicit cancellation — ended the run.
    /// Task 23A owns the full Go text `run deadline of <duration> reached` (it
    /// holds the configured `run_deadline` and formats it with
    /// `format_go_duration`); this just identifies the cause.
    pub fn deadline_reason(&self) -> Option<String> {
        let deadline = self.deadline?;
        if tokio::time::Instant::now() >= deadline {
            Some("run deadline reached".to_string())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn child_cancels_with_parent_but_not_vice_versa() {
        // Cancelling a child must not cancel the parent.
        let parent = RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        };
        let child = parent.child();
        child.cancel.cancel();
        assert!(child.cancel.is_cancelled());
        assert!(!parent.cancel.is_cancelled());

        // A live child is cancelled when its parent is cancelled.
        let parent2 = RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        };
        let child2 = parent2.child();
        parent2.cancel.cancel();
        assert!(parent2.cancel.is_cancelled());
        assert!(child2.cancel.is_cancelled());
    }

    #[test]
    fn child_inherits_deadline() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let parent = RunContext {
            cancel: CancellationToken::new(),
            deadline: Some(deadline),
        };
        let child = parent.child();
        assert_eq!(child.deadline, Some(deadline));
    }

    #[test]
    fn deadline_reason_reflects_elapsed_deadline() {
        let none = RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        };
        assert!(none.deadline_reason().is_none());

        let future = RunContext {
            cancel: CancellationToken::new(),
            deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(60)),
        };
        assert!(future.deadline_reason().is_none());

        let past = RunContext {
            cancel: CancellationToken::new(),
            deadline: Some(tokio::time::Instant::now() - std::time::Duration::from_secs(1)),
        };
        assert!(past.deadline_reason().is_some());
    }
}
