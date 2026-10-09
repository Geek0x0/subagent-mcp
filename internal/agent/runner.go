package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/Geek0x0/subagent-mcp/internal/patch"
	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/tools"

	"github.com/google/uuid"
)

var ErrBusy = errors.New("thread is busy")

// cancelledToolResult is the tool result recorded for calls skipped after the
// context was cancelled, so every tool call in the history keeps a matching
// result and providers still accept the history on the next reply.
const cancelledToolResult = "cancelled: not executed"

// continuePrompt answers a reply that has no tool call when nudging is enabled.
// Some models narrate their next step ("Now step 2: write the failing test")
// and stop; a reply without a tool call ends the run, so the narration would be
// returned as the final answer while the work is unfinished.
const continuePrompt = "You replied without a tool call, which ends the run. " +
	"If any work remains, continue now by making the tool call. " +
	"If the task is completely done, repeat your final summary."

// continueAfterEmptyPrompt answers a reply with no text and no tool call. An
// API can drop a tool call it fails to stream (the reply then arrives empty
// although the model produced tokens), and asking for the same call again tends
// to lose it the same way, so the prompt asks for a smaller or different one.
const continueAfterEmptyPrompt = "Your last reply was empty: it had no text and no tool call, " +
	"so any tool call you made may not have been delivered. " +
	"If work remains, make the next tool call now, and keep it small and simple " +
	"(for example split a large file write into several smaller calls). " +
	"If the task is completely done, reply with your final summary."

type Emitter interface {
	Emit(ctx context.Context, threadID string, msg map[string]any)
}

type ApprovalRequest struct {
	Tool    string
	Command string
	Path    string
	Reason  string
}

type Approver interface {
	Approve(ctx context.Context, threadID string, req ApprovalRequest) bool
}

type Runner struct {
	Emitter  Emitter
	Approver Approver
}

func BuiltinTools() []provider.ToolSpec {
	return []provider.ToolSpec{
		{
			Name: "shell",
			Description: "Run a bash command in the working directory. The sandbox policy may deny the call; " +
				"providing justification helps if approval is required.",
			Parameters: json.RawMessage(`{
				"type": "object",
				"properties": {
					"command": {"type": "string", "description": "Bash command to run."},
					"timeout_seconds": {"type": "integer", "description": "Optional timeout in seconds (clamped, max 600)."},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["command"]
			}`),
		},
		{
			Name: "read_file",
			Description: "Read a file, resolving relative paths against the working directory. The sandbox policy may deny the call; " +
				"providing justification helps if approval is required.",
			Parameters: json.RawMessage(`{
				"type": "object",
				"properties": {
					"path": {"type": "string", "description": "File path to read."},
					"offset": {"type": "integer", "description": "1-based line number to start reading from; defaults to 1"},
					"limit": {"type": "integer", "description": "maximum number of lines to return; defaults to all that fit in the 16 KiB output cap"},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["path"]
			}`),
		},
		{
			Name: "write_file",
			Description: "Create or overwrite a whole file, creating parent directories as needed. The sandbox policy may deny the call; " +
				"providing justification helps if approval is required.",
			Parameters: json.RawMessage(`{
				"type": "object",
				"properties": {
					"path": {"type": "string", "description": "File path to write."},
					"content": {"type": "string", "description": "Complete file content."},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["path", "content"]
			}`),
		},
		{
			Name: "apply_patch",
			Description: "Edit files with a patch: '*** Begin Patch', then '*** Add File: <path>' (+lines), " +
				"'*** Delete File: <path>', or '*** Update File: <path>' (optional '*** Move to: <path>') with '@@' chunks " +
				"of ' ' context, '-' removed, and '+' added lines, then '*** End Patch'. Prefer this for editing existing files. " +
				"The sandbox policy may deny the call; providing justification helps if approval is required.",
			Parameters: json.RawMessage(`{
				"type": "object",
				"properties": {
					"patch": {"type": "string", "description": "Complete patch text from *** Begin Patch to *** End Patch."},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["patch"]
			}`),
		},
	}
}

func (r *Runner) Run(ctx context.Context, s *Session, prompt string) (string, error) {
	// ponytail: This lock spans model, approval, tool, and emitter calls, so a hung dependency leaves the thread busy;
	// per-phase state locking plus a caller-visible cancel/abort channel would remove that ceiling.
	if !s.mu.TryLock() {
		return "", ErrBusy
	}
	defer s.mu.Unlock()
	return r.RunLocked(ctx, s, prompt)
}

// RunLocked is Run for a session the caller already holds through
// Manager.Acquire; it neither takes nor releases the session lock.
func (r *Runner) RunLocked(ctx context.Context, s *Session, prompt string) (string, error) {
	defer func() { s.setLastUsed(time.Now()) }()

	// One clock covers every way a run blocks — model request, approval, shell —
	// so a caller-set deadline cannot be outlasted by a phase that ignores it.
	if s.runDeadline > 0 {
		var stop context.CancelFunc
		ctx, stop = context.WithTimeoutCause(ctx, s.runDeadline, fmt.Errorf("run deadline of %s reached", s.runDeadline))
		defer stop()
	}

	if s.thread != nil {
		// A delegated thread spends the same budget as the built-in loop, so the
		// check runs before the thread is handed the prompt. Both ceilings are
		// cumulative, which is why a thread that reaches one stays stopped.
		if err := s.budgetError(); err != nil {
			r.Emitter.Emit(ctx, s.ID, map[string]any{"type": "error", "message": err.Error()})
			recordError(s, err)
			return "", err
		}
		text, err := s.thread.Run(ctx, prompt, s.effortSent, provider.ThreadCallbacks{
			Emit: func(event map[string]any) {
				r.recordThreadEvent(s, event)
				r.Emitter.Emit(ctx, s.ID, event)
			},
			Approve: func(approveCtx context.Context, req provider.ApprovalRequest) bool {
				return r.Approver.Approve(approveCtx, s.ID, ApprovalRequest{
					Tool: req.Tool, Command: req.Command, Path: req.Path, Reason: req.Reason,
				})
			},
		})
		if err != nil {
			if ctx.Err() != nil && errors.Is(err, ctx.Err()) {
				err = runCause(ctx)
			}
			r.Emitter.Emit(ctx, s.ID, map[string]any{"type": "error", "message": err.Error()})
			return "", err
		}
		return text, nil
	}

	r.Emitter.Emit(ctx, s.ID, map[string]any{"type": "task_started"})
	started := time.Now()
	s.turnID = uuid.NewString()
	recordTurnStart(s, prompt, started)
	entryMessages := len(s.messages)
	s.appendMessage(provider.Message{Role: provider.RoleUser, Text: prompt})
	assistantAppended := false
	nudges := 0
	nudgedSinceTool := false
	lastText := ""

	for turn := 0; turn < s.maxTurns; turn++ {
		// No history rollback here: both ceilings are cumulative for the thread's
		// life, so a thread that reached one cannot run again whatever its
		// history holds. The loop's provider-failure path rolls back because
		// that run is resumable; this one is not.
		if err := s.budgetError(); err != nil {
			r.Emitter.Emit(ctx, s.ID, map[string]any{"type": "error", "message": err.Error()})
			recordError(s, err)
			return "", err
		}
		requestStarted := time.Now()
		var firstDelta time.Time
		// One notification per streamed token floods the client's bounded
		// notification channel, and mcp-go drops rather than blocks when it
		// fills, so most of a long reply would never arrive. Deltas are merged
		// into one event per interval or size, and the tail is flushed as soon
		// as the turn returns; the concatenation a client sees is unchanged.
		deltas := newDeltaCoalescer(func(text string) {
			r.Emitter.Emit(ctx, s.ID, map[string]any{
				"type":  "agent_message_delta",
				"delta": text,
			})
		})
		res, err := s.provider.Turn(ctx, provider.TurnRequest{
			Model:    s.model,
			Effort:   s.effortSent,
			System:   s.system,
			Messages: s.messages,
			Tools:    BuiltinTools(),
		}, func(delta string) {
			if firstDelta.IsZero() {
				firstDelta = time.Now()
			}
			deltas.add(delta)
		})
		deltas.flush()
		requestFinished := time.Now()
		if err != nil && ctx.Err() != nil && errors.Is(err, ctx.Err()) {
			// A deadline or a cancellation reaches the adapter as its request's
			// context error; report the cause so a run deadline names itself
			// rather than surfacing as a bare "context deadline exceeded".
			err = runCause(ctx)
		}
		requestEvent := map[string]any{
			"type":        "provider_request",
			"provider":    s.provider.Name(),
			"model":       s.model,
			"duration_ms": requestFinished.Sub(requestStarted).Milliseconds(),
		}
		if !firstDelta.IsZero() {
			requestEvent["ttft_ms"] = firstDelta.Sub(requestStarted).Milliseconds()
		}
		if err != nil {
			requestEvent["error"] = err.Error()
		} else if res.Upstream != "" {
			requestEvent["upstream"] = res.Upstream
		}
		recordProviderRequest(s, requestEvent)
		r.Emitter.Emit(ctx, s.ID, requestEvent)
		if err != nil {
			if !assistantAppended {
				// No assistant message was appended during this Run, so the
				// prompt never produced a reply. Restore the history to its
				// entry length: a failed or cancelled first model call must
				// not leave the prompt to be replayed by the next reply.
				s.truncateMessages(entryMessages)
			}
			r.Emitter.Emit(ctx, s.ID, map[string]any{
				"type":    "error",
				"message": err.Error(),
			})
			recordError(s, err)
			return "", err
		}
		if res.Usage != nil {
			r.Emitter.Emit(ctx, s.ID, map[string]any{
				"type":              "token_count",
				"prompt_tokens":     res.Usage.Input,
				"completion_tokens": res.Usage.Output,
				"total_tokens":      res.Usage.Total,
			})
		}
		recordModelTurn(s, res)
		if len(res.ToolCalls) == 0 && res.Text == "" {
			// An empty reply is not replayed: several APIs reject an assistant
			// message with neither content nor tool calls.
		} else {
			s.appendMessage(provider.Message{
				Role: provider.RoleAssistant, Text: res.Text, ToolCalls: res.ToolCalls, Opaque: res.Opaque,
			})
		}
		if len(res.ToolCalls) == 0 {
			if res.Text != "" {
				lastText = res.Text
			}
			if nudges < s.maxNudges && (!nudgedSinceTool || res.Text == "") {
				// Ask once per stretch without tool calls: a model that was
				// really finished repeats its summary, and that second reply
				// is accepted as final. An empty reply is never a final answer
				// (some models end a turn with only hidden reasoning), so it is
				// asked again while the budget lasts.
				nudges++
				nudgedSinceTool = true
				assistantAppended = true
				prompt := continuePrompt
				if res.Text == "" {
					prompt = continueAfterEmptyPrompt
				}
				s.appendMessage(provider.Message{Role: provider.RoleUser, Text: prompt})
				r.Emitter.Emit(ctx, s.ID, map[string]any{"type": "agent_nudge", "message": prompt})
				continue
			}
			text := res.Text
			if text == "" {
				text = lastText
			}
			r.Emitter.Emit(ctx, s.ID, map[string]any{
				"type":    "agent_message",
				"message": text,
			})
			r.Emitter.Emit(ctx, s.ID, map[string]any{"type": "task_complete"})
			recordTaskComplete(s, text, started)
			return text, nil
		}
		assistantAppended = true
		nudgedSinceTool = false

		for _, call := range res.ToolCalls {
			if ctxErr := ctx.Err(); ctxErr != nil {
				// The context was cancelled: this call must neither execute
				// nor ask for approval, but it still gets an error tool
				// result so every tool call keeps a matching result in the
				// history. The context error is returned after the batch.
				recordToolCall(s, call)
				recordToolOutput(s, call, cancelledToolResult)
				s.appendMessage(provider.Message{
					Role: provider.RoleTool, ToolCallID: call.ID, Text: cancelledToolResult, IsError: true,
				})
				continue
			}
			recordToolCall(s, call)
			content, isError := r.safeExecToolCall(ctx, s, call)
			if content == "" {
				content = "(empty output)"
			}
			recordToolOutput(s, call, content)
			s.appendMessage(provider.Message{
				Role: provider.RoleTool, ToolCallID: call.ID, Text: content, IsError: isError,
			})
		}
		if ctxErr := ctx.Err(); ctxErr != nil {
			// Report the cause when there is one: a run deadline set here is
			// more useful to the caller than a bare context error.
			err := runCause(ctx)
			r.Emitter.Emit(ctx, s.ID, map[string]any{
				"type":    "error",
				"message": err.Error(),
			})
			recordError(s, err)
			return "", err
		}
	}

	err := fmt.Errorf("turn limit reached (%d) without a final answer", s.maxTurns)
	r.Emitter.Emit(ctx, s.ID, map[string]any{
		"type":    "error",
		"message": err.Error(),
	})
	recordError(s, err)
	return "", err
}

// Bounds on how long a streamed delta waits for company before it is emitted as
// one agent_message_delta event.
const (
	deltaFlushInterval = 50 * time.Millisecond
	deltaFlushBytes    = 8 << 10
)

// deltaCoalescer merges streamed text deltas into fewer events. It is used from
// the provider's streaming callback and flushed by the caller that owns it, so
// it needs no locking.
type deltaCoalescer struct {
	emit    func(string)
	pending strings.Builder
	last    time.Time
	// interval is how long a delta waits for company before it is emitted on its
	// own. It is deltaFlushInterval in production, and a test that needs buffering
	// to be deterministic sets it longer than the test can possibly run.
	interval time.Duration
}

func newDeltaCoalescer(emit func(string)) *deltaCoalescer {
	return &deltaCoalescer{emit: emit, last: time.Now(), interval: deltaFlushInterval}
}

func (d *deltaCoalescer) add(delta string) {
	d.pending.WriteString(delta)
	if d.pending.Len() >= deltaFlushBytes || time.Since(d.last) >= d.interval {
		d.flush()
	}
}

func (d *deltaCoalescer) flush() {
	if d.pending.Len() == 0 {
		return
	}
	text := d.pending.String()
	d.pending.Reset()
	d.last = time.Now()
	d.emit(text)
}

// budgetError reports whether the thread has spent its configured budget, so the
// next model call is refused before it costs anything. Both budgets are checked
// here rather than at the call site because a turn is the unit that spends.
func (s *Session) budgetError() error {
	if s.maxHistoryBytes > 0 && s.historyBytes > s.maxHistoryBytes {
		return fmt.Errorf(
			"thread history reached %d bytes (max_history_bytes %d); start a new thread",
			s.historyBytes, s.maxHistoryBytes,
		)
	}
	if s.maxTotalTokens > 0 && s.totalUsage.Total >= s.maxTotalTokens {
		return fmt.Errorf(
			"thread spent %d tokens (max_total_tokens %d)",
			s.totalUsage.Total, s.maxTotalTokens,
		)
	}
	return nil
}

// recordThreadEvent accounts the usage a delegated thread reports, so the token
// budget covers providers that run whole threads and never return a TurnResult.
//
// The total is the sum of what the thread reports per turn. A provider that
// reports the same work twice would make this an over-count, which is the safe
// direction for a ceiling — the thread stops early rather than late.
func (r *Runner) recordThreadEvent(s *Session, event map[string]any) {
	if event["type"] != "token_count" {
		return
	}
	last := provider.Usage{
		Input:  intField(event, "prompt_tokens"),
		Output: intField(event, "completion_tokens"),
		Total:  intField(event, "total_tokens"),
	}
	s.addUsage(last)
	// The rollout line the built-in loop writes from recordModelTurn is written
	// here instead: a delegated thread reports usage through these events and has
	// no TurnResult to take it from, so without this its rollout records no
	// tokens at all.
	s.rollout.Event(map[string]any{"type": "token_count", "info": map[string]any{
		"total_token_usage": usagePayload(s.totalUsage),
		"last_token_usage":  usagePayload(last),
	}})
}

// intField reads one numeric field of a token_count event. The events a provider
// adapter builds in process carry Go ints, while the same event decoded from
// JSON carries float64, so both have to be accepted.
func intField(event map[string]any, key string) int {
	switch value := event[key].(type) {
	case int:
		return value
	case int64:
		return int(value)
	case float64:
		return int(value)
	default:
		return 0
	}
}

// runCause returns the most informative error for a finished run context: the
// cause when one was set (a run deadline), else the plain context error.
func runCause(ctx context.Context) error {
	if cause := context.Cause(ctx); cause != nil {
		return cause
	}
	return ctx.Err()
}

func (r *Runner) safeExecToolCall(ctx context.Context, s *Session, toolCall provider.ToolCall) (result string, isError bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			result = fmt.Sprintf("internal error: tool execution panicked: %v", recovered)
			isError = true
		}
	}()

	return r.execToolCall(ctx, s, toolCall)
}

func (r *Runner) execToolCall(ctx context.Context, s *Session, toolCall provider.ToolCall) (string, bool) {
	if ctxErr := ctx.Err(); ctxErr != nil {
		return cancelledToolResult, true
	}
	var args struct {
		Command        string `json:"command"`
		TimeoutSeconds int    `json:"timeout_seconds"`
		Path           string `json:"path"`
		Offset         int    `json:"offset"`
		Limit          int    `json:"limit"`
		Content        string `json:"content"`
		Patch          string `json:"patch"`
		Justification  string `json:"justification"`
	}
	if err := json.Unmarshal([]byte(toolCall.Arguments), &args); err != nil {
		return "invalid tool arguments: " + err.Error() + "; received: " + truncateArguments(toolCall.Arguments, 200), true
	}

	switch toolCall.Name {
	case "shell", "read_file", "write_file", "apply_patch":
	default:
		return "unknown tool: " + toolCall.Name, true
	}
	if s.sandbox == policy.Sandbox("workspace-write") && (toolCall.Name == "write_file" || toolCall.Name == "apply_patch") {
		if s.bindErr != nil {
			return "error: " + s.bindErr.Error(), true
		}
		if s.boundCwd == nil {
			return "error: working directory is not bound", true
		}
		if err := s.boundCwd.Check(); err != nil {
			return "error: " + err.Error(), true
		}
	}

	requests := []policy.Request{{Tool: toolCall.Name, Command: args.Command, Path: args.Path, Cwd: s.cwd, BoundCwd: s.boundCwd}}
	var hunks []patch.Hunk
	var paths []string
	if toolCall.Name == "apply_patch" {
		parsed, err := patch.Parse(args.Patch)
		if err != nil {
			return "error: invalid patch: " + err.Error(), true
		}
		hunks = parsed
		paths = patch.Paths(hunks)
		requests = requests[:0]
		for _, path := range paths {
			requests = append(requests, policy.Request{Tool: "write_file", Path: path, Cwd: s.cwd, BoundCwd: s.boundCwd})
		}
	}
	approved, denial := r.authorize(ctx, s, toolCall.Name, args.Command, args.Justification, requests)
	if denial != "" {
		return denial, true
	}

	beginEvent := map[string]any{
		"type":    "exec_command_begin",
		"call_id": toolCall.ID,
		"tool":    toolCall.Name,
	}
	switch toolCall.Name {
	case "shell":
		beginEvent["command"] = args.Command
	case "apply_patch":
		beginEvent["paths"] = paths
	default:
		beginEvent["path"] = args.Path
	}

	var exitCode *int
	if toolCall.Name == "shell" {
		unknownExitCode := -1
		exitCode = &unknownExitCode
	}
	var toolErr error
	defer func() {
		r.emitExecEnd(ctx, s, toolCall.ID, toolCall.Name, exitCode, toolErr, paths)
	}()
	r.Emitter.Emit(ctx, s.ID, beginEvent)

	switch toolCall.Name {
	case "shell":
		timeout := time.Duration(args.TimeoutSeconds) * time.Second
		var out string
		var code int
		var err error
		if _, sandboxed := s.shellWritableRoots(); sandboxed && !approved {
			if s.bindErr != nil {
				err, code = s.bindErr, -1
			} else {
				out, code, err = tools.RunShellSandboxedBound(ctx, args.Command, timeout, s.boundCwd, s.boundRoots)
			}
		} else {
			// ponytail: A human-approved command runs without the kernel sandbox, matching Codex escalation.
			out, code, err = tools.RunShell(ctx, s.cwd, args.Command, timeout)
		}
		*exitCode = code
		toolErr = err

		result := fmt.Sprintf("exit code: %d\n%s", code, out)
		if err != nil {
			result += fmt.Sprintf("\n[error: %s]", err)
		}
		return result, err != nil

	case "read_file":
		content, err := tools.ReadFileRange(ctx, s.cwd, args.Path, args.Offset, args.Limit)
		toolErr = err

		if err != nil {
			return "error: " + err.Error(), true
		}
		return content, false

	case "write_file":
		path := args.Path
		var confined patch.FS
		if s.sandbox == policy.Sandbox("workspace-write") && !approved {
			var err error
			path, err = policy.BoundPath(s.cwd, args.Path, s.boundCwd)
			if err != nil {
				toolErr = err
				return "error: " + err.Error(), true
			}
			// The kernel keeps the write beneath the bound directory even if a
			// path component is swapped for a symlink after the check above.
			confined = s.boundCwd.FS()
		}
		var err error
		if confined != nil {
			err = confined.WriteFile(path, []byte(args.Content), 0o644)
		} else {
			err = tools.WriteFile(s.cwd, path, args.Content)
		}
		toolErr = err

		if err != nil {
			return "error: " + err.Error(), true
		}
		return fmt.Sprintf("wrote %d bytes to %s", len(args.Content), args.Path), false

	case "apply_patch":
		planned := hunks
		fsys := patch.OS
		if s.sandbox == policy.Sandbox("workspace-write") && !approved {
			planned = append([]patch.Hunk(nil), hunks...)
			for i := range planned {
				var err error
				planned[i].Path, err = policy.BoundPath(s.cwd, hunks[i].Path, s.boundCwd)
				if err == nil && hunks[i].MoveTo != "" {
					planned[i].MoveTo, err = policy.BoundPath(s.cwd, hunks[i].MoveTo, s.boundCwd)
				}
				if err != nil {
					toolErr = err
					return "error: " + err.Error(), true
				}
			}
			// The kernel keeps every read and write beneath the bound directory
			// even if a path component is swapped for a symlink after the checks.
			if confined := s.boundCwd.FS(); confined != nil {
				fsys = confined
			}
		}
		changes, err := patch.PlanFS(fsys, s.cwd, planned)
		if err == nil {
			err = patch.CommitFS(fsys, changes)
		}
		toolErr = err
		if err != nil {
			result := "error: " + err.Error()
			recordPatchApplied(s, toolCall.ID, nil, result, err)
			return result, true
		}
		if planned != nil && len(planned) > 0 && s.sandbox == policy.Sandbox("workspace-write") && !approved {
			for i := range changes {
				changes[i].Path = displayPath(s.cwd, hunks[i].Path)
				if hunks[i].MoveTo != "" {
					changes[i].MoveTo = displayPath(s.cwd, hunks[i].MoveTo)
				}
			}
		}
		result := patch.Summary(changes)
		recordPatchApplied(s, toolCall.ID, changes, result, nil)
		return result, false
	}

	return "unknown tool: " + toolCall.Name, true
}

func displayPath(cwd, path string) string {
	if filepath.IsAbs(path) {
		return filepath.Clean(path)
	}
	return filepath.Join(cwd, path)
}

// truncateArguments returns raw limited to max bytes on a UTF-8 boundary,
// appending an ellipsis when it had to cut.
func truncateArguments(raw string, max int) string {
	if len(raw) <= max {
		return raw
	}
	cut := max
	for cut > 0 && !utf8.RuneStart(raw[cut]) {
		cut--
	}
	return raw[:cut] + "…"
}

// authorize evaluates every request; any Deny rejects the call, and all
// approval-requiring requests are combined into one approval prompt.
func (r *Runner) authorize(
	ctx context.Context,
	s *Session,
	tool, command, justification string,
	requests []policy.Request,
) (approved bool, denial string) {
	var reasons, paths []string
	for _, req := range requests {
		decision, reason := policy.Evaluate(s.sandbox, s.approval, req)
		switch decision {
		case policy.Deny:
			return false, "operation denied by sandbox policy: " + reason
		case policy.AskApproval:
			reasons = append(reasons, reason)
			if req.Path != "" {
				paths = append(paths, req.Path)
			}
		}
	}
	if len(reasons) == 0 {
		return false, ""
	}
	approvalReason := strings.Join(reasons, "; ")
	if justification != "" {
		approvalReason = fmt.Sprintf("%s (model justification: %s)", approvalReason, justification)
	}
	// Say what approving actually does: an approved shell command skips the
	// kernel sandbox, which is not visible from the command itself. (A git spelled
	// around the wrapper also skips the hardening auto-allowed git gets; that is
	// documented in the README rather than claimed here.)
	if escalations := s.escalationsFor(tool); escalations != "" {
		approvalReason = fmt.Sprintf("%s [approving this %s]", approvalReason, escalations)
	}
	if !r.Approver.Approve(ctx, s.ID, ApprovalRequest{
		Tool:    tool,
		Command: command,
		Path:    strings.Join(paths, ", "),
		Reason:  approvalReason,
	}) {
		return false, "operation denied: approval was not granted"
	}
	return true, ""
}

// escalationsFor describes what approving this tool bypasses, so the approval
// prompt names it. An approved shell command runs without the kernel sandbox in
// every sandbox except danger-full-access, which was never going to sandbox it.
func (s *Session) escalationsFor(tool string) string {
	if tool != "shell" {
		return ""
	}
	if _, sandboxed := s.shellWritableRoots(); !sandboxed {
		return ""
	}
	return "runs the command without the kernel sandbox"
}

func (r *Runner) emitExecEnd(
	ctx context.Context,
	s *Session,
	callID string,
	tool string,
	exitCode *int,
	err error,
	paths []string,
) {
	endEvent := map[string]any{
		"type":    "exec_command_end",
		"call_id": callID,
		"tool":    tool,
	}
	if exitCode != nil {
		endEvent["exit_code"] = *exitCode
	}
	if paths != nil {
		endEvent["paths"] = paths
	}
	if err != nil {
		endEvent["error"] = err.Error()
	}
	r.Emitter.Emit(ctx, s.ID, endEvent)
}
