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

	if s.thread != nil {
		text, err := s.thread.Run(ctx, prompt, s.effortSent, provider.ThreadCallbacks{
			Emit: func(event map[string]any) {
				r.Emitter.Emit(ctx, s.ID, event)
			},
			Approve: func(approveCtx context.Context, req provider.ApprovalRequest) bool {
				return r.Approver.Approve(approveCtx, s.ID, ApprovalRequest{
					Tool: req.Tool, Command: req.Command, Path: req.Path, Reason: req.Reason,
				})
			},
		})
		if err != nil {
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
	s.messages = append(s.messages, provider.Message{Role: provider.RoleUser, Text: prompt})
	assistantAppended := false
	nudges := 0
	nudgedSinceTool := false
	lastText := ""

	for turn := 0; turn < s.maxTurns; turn++ {
		requestStarted := time.Now()
		var firstDelta time.Time
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
			r.Emitter.Emit(ctx, s.ID, map[string]any{
				"type":  "agent_message_delta",
				"delta": delta,
			})
		})
		requestFinished := time.Now()
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
		}
		recordProviderRequest(s, requestEvent)
		r.Emitter.Emit(ctx, s.ID, requestEvent)
		if err != nil {
			if !assistantAppended {
				// No assistant message was appended during this Run, so the
				// prompt never produced a reply. Restore the history to its
				// entry length: a failed or cancelled first model call must
				// not leave the prompt to be replayed by the next reply.
				s.messages = s.messages[:entryMessages]
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
			s.messages = append(s.messages, provider.Message{
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
				s.messages = append(s.messages, provider.Message{Role: provider.RoleUser, Text: prompt})
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
				s.messages = append(s.messages, provider.Message{
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
			s.messages = append(s.messages, provider.Message{
				Role: provider.RoleTool, ToolCallID: call.ID, Text: content, IsError: isError,
			})
		}
		if ctxErr := ctx.Err(); ctxErr != nil {
			r.Emitter.Emit(ctx, s.ID, map[string]any{
				"type":    "error",
				"message": ctxErr.Error(),
			})
			recordError(s, ctxErr)
			return "", ctxErr
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
