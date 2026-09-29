package agent

import (
	"encoding/json"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/patch"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func addUsage(a, b provider.Usage) provider.Usage {
	return provider.Usage{
		Input:     a.Input + b.Input,
		Cached:    a.Cached + b.Cached,
		Output:    a.Output + b.Output,
		Reasoning: a.Reasoning + b.Reasoning,
		Total:     a.Total + b.Total,
	}
}

func usagePayload(u provider.Usage) map[string]any {
	return map[string]any{
		"input_tokens":            u.Input,
		"cached_input_tokens":     u.Cached,
		"output_tokens":           u.Output,
		"reasoning_output_tokens": u.Reasoning,
		"total_tokens":            u.Total,
	}
}

func textContent(kind, text string) []any {
	return []any{map[string]any{"type": kind, "text": text}}
}

func (s *Session) sandboxPolicy() map[string]any {
	roots, _ := s.shellWritableRoots()
	return map[string]any{"type": string(s.sandbox), "writable_roots": roots}
}

func recordTurnStart(s *Session, prompt string, started time.Time) {
	s.rollout.Write("turn_context", map[string]any{
		"turn_id":         s.turnID,
		"cwd":             s.cwd,
		"approval_policy": string(s.approval),
		"sandbox_policy":  s.sandboxPolicy(),
		"model":           s.model,
		"effort":          s.reasoningEffort,
		"effort_sent":     s.effortSent,
		"current_date":    started.Format("2006-01-02"),
	})
	s.rollout.Event(map[string]any{"type": "task_started", "turn_id": s.turnID, "started_at": started.Unix()})
	s.rollout.Item(map[string]any{"type": "message", "role": "user", "content": textContent("input_text", prompt)})
	s.rollout.Event(map[string]any{"type": "user_message", "message": prompt})
}

func recordModelTurn(s *Session, res *provider.TurnResult) {
	if res.Reasoning != "" {
		s.rollout.Item(map[string]any{"type": "reasoning", "summary": []any{}, "content": textContent("reasoning_text", res.Reasoning)})
	}
	if res.Usage != nil {
		last := *res.Usage
		s.totalUsage = addUsage(s.totalUsage, last)
		s.rollout.Event(map[string]any{"type": "token_count", "info": map[string]any{
			"total_token_usage": usagePayload(s.totalUsage),
			"last_token_usage":  usagePayload(last),
		}})
	}
	if res.Text != "" {
		s.rollout.Item(map[string]any{"type": "message", "role": "assistant", "content": textContent("output_text", res.Text)})
		s.rollout.Event(map[string]any{"type": "agent_message", "message": res.Text})
	}
}

func recordToolCall(s *Session, call provider.ToolCall) {
	if call.Name == "apply_patch" {
		var args struct {
			Patch string `json:"patch"`
		}
		_ = json.Unmarshal([]byte(call.Arguments), &args)
		s.rollout.Item(map[string]any{"type": "custom_tool_call", "status": "completed", "call_id": call.ID, "name": "apply_patch", "input": args.Patch})
		return
	}
	s.rollout.Item(map[string]any{"type": "function_call", "call_id": call.ID, "name": call.Name, "arguments": call.Arguments})
}

func recordToolOutput(s *Session, call provider.ToolCall, output string) {
	itemType := "function_call_output"
	if call.Name == "apply_patch" {
		itemType = "custom_tool_call_output"
	}
	s.rollout.Item(map[string]any{"type": itemType, "call_id": call.ID, "output": output})
}

func recordPatchApplied(s *Session, callID string, changes []patch.Change, output string, err error) {
	payload := map[string]any{"type": "patch_apply_end", "call_id": callID, "turn_id": s.turnID, "success": err == nil, "stdout": "", "stderr": ""}
	if err != nil {
		payload["stderr"] = output
	} else {
		payload["stdout"] = output
	}
	byPath := map[string]any{}
	for _, change := range changes {
		entry := map[string]any{"type": change.Kind.String(), "move_path": nil}
		switch change.Kind {
		case patch.Add:
			entry["content"] = change.NewContent
		case patch.Update:
			entry["unified_diff"] = change.Diff
			if change.MoveTo != "" {
				entry["move_path"] = change.MoveTo
			}
		}
		byPath[change.Path] = entry
	}
	payload["changes"] = byPath
	s.rollout.Event(payload)
}

func recordTaskComplete(s *Session, message string, started time.Time) {
	completed := time.Now()
	s.rollout.Event(map[string]any{
		"type":               "task_complete",
		"turn_id":            s.turnID,
		"last_agent_message": message,
		"started_at":         started.Unix(),
		"completed_at":       completed.Unix(),
		"duration_ms":        completed.Sub(started).Milliseconds(),
	})
}

func recordProviderRequest(s *Session, payload map[string]any) {
	s.rollout.Event(payload)
}

func recordError(s *Session, err error) {
	s.rollout.Event(map[string]any{"type": "error", "message": err.Error()})
}
