package provider

import "context"

// Agent is an optional capability for providers that run whole threads with
// their own tools, sandbox, and approvals instead of individual model turns.
type Agent interface {
	StartThread(ctx context.Context, opts ThreadOptions) (Thread, error)
}

// Thread is a backend-owned conversation that can run successive prompts.
type Thread interface {
	// Run executes one turn and returns the final reply text.
	Run(ctx context.Context, prompt, effort string, cb ThreadCallbacks) (string, error)
	// Close releases backend resources; best effort, never blocks long.
	Close()
}

// ThreadOptions carries subagent-mcp values; the backend owns wire mappings.
type ThreadOptions struct {
	Model, Cwd, Sandbox, ApprovalPolicy     string
	BaseInstructions, DeveloperInstructions string // empty = backend default / none
	WritableRoots                           []string
	Ephemeral                               bool
}

// ThreadCallbacks connects backend events and approvals to the session caller.
type ThreadCallbacks struct {
	Emit func(event map[string]any)
	// Approve receives a context that is cancelled when the turn can no
	// longer wait for a decision: the caller's context, a crashed child, or a
	// closed thread. Implementations must stop waiting when it ends.
	Approve func(ctx context.Context, req ApprovalRequest) bool
}

// ApprovalRequest describes an operation that needs the caller's approval.
type ApprovalRequest struct{ Tool, Command, Path, Reason string }

// AuthStatus describes backend authentication and version information.
type AuthStatus struct{ Summary, Version string }

// AuthChecker is an optional capability for checking backend authentication.
type AuthChecker interface {
	CheckAuth(ctx context.Context) (AuthStatus, error)
}
