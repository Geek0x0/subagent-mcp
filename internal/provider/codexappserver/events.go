package codexappserver

import "encoding/json"

type codexItem struct {
	ID       string `json:"id"`
	Type     string `json:"type"`
	Text     string `json:"text"`
	Phase    string `json:"phase"`
	Command  string `json:"command"`
	ExitCode *int   `json:"exitCode"`
	Changes  []struct {
		Path string `json:"path"`
	} `json:"changes"`
}

func (i codexItem) paths() []string {
	paths := make([]string, 0, len(i.Changes))
	for _, change := range i.Changes {
		paths = append(paths, change.Path)
	}
	return paths
}

type notificationParams struct {
	ThreadID string    `json:"threadId"`
	TurnID   string    `json:"turnId"`
	Delta    string    `json:"delta"`
	Item     codexItem `json:"item"`
	Turn     struct {
		ID     string `json:"id"`
		Status string `json:"status"`
		Error  struct {
			Message string `json:"message"`
		} `json:"error"`
	} `json:"turn"`
	TokenUsage struct {
		Last struct {
			InputTokens  int `json:"inputTokens"`
			OutputTokens int `json:"outputTokens"`
			TotalTokens  int `json:"totalTokens"`
		} `json:"last"`
	} `json:"tokenUsage"`
	Error struct {
		Message string `json:"message"`
	} `json:"error"`
	WillRetry bool `json:"willRetry"`
}

func mapNotification(method string, params json.RawMessage) (event map[string]any, ok bool) {
	var p notificationParams
	if json.Unmarshal(params, &p) != nil {
		return nil, false
	}
	switch method {
	case "turn/started":
		return map[string]any{"type": "task_started"}, true
	case "turn/completed":
		return map[string]any{"type": "task_complete"}, true
	case "item/agentMessage/delta":
		return map[string]any{"type": "agent_message_delta", "delta": p.Delta}, true
	case "item/started", "item/completed":
		if p.Item.Type == "agentMessage" && method == "item/completed" {
			return map[string]any{"type": "agent_message", "message": p.Item.Text}, true
		}
		if p.Item.Type != "commandExecution" && p.Item.Type != "fileChange" {
			return nil, false
		}
		event = map[string]any{"type": "exec_command_begin", "call_id": p.Item.ID}
		if method == "item/completed" {
			event["type"] = "exec_command_end"
		}
		if p.Item.Type == "fileChange" {
			event["tool"], event["paths"] = "apply_patch", p.Item.paths()
		} else {
			event["tool"] = "shell"
			if method == "item/started" {
				event["command"] = p.Item.Command
			} else if p.Item.ExitCode != nil {
				event["exit_code"] = *p.Item.ExitCode
			}
		}
		return event, true
	case "thread/tokenUsage/updated":
		last := p.TokenUsage.Last
		return map[string]any{
			"type": "token_count", "prompt_tokens": last.InputTokens,
			"completion_tokens": last.OutputTokens, "total_tokens": last.TotalTokens,
		}, true
	case "error":
		if !p.WillRetry {
			return map[string]any{"type": "error", "message": p.Error.Message}, true
		}
	}
	return nil, false
}
