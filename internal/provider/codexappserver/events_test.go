package codexappserver

import (
	"encoding/json"
	"reflect"
	"testing"
)

func TestMapNotification(t *testing.T) {
	for _, tc := range []struct {
		name, method, params string
		want                 map[string]any
	}{
		{"started", "turn/started", `{"turn":{"id":"t"}}`, map[string]any{"type": "task_started"}},
		{"delta", "item/agentMessage/delta", `{"delta":"hi"}`, map[string]any{"type": "agent_message_delta", "delta": "hi"}},
		{"message", "item/completed", `{"item":{"type":"agentMessage","text":"42","phase":"final_answer"}}`, map[string]any{"type": "agent_message", "message": "42"}},
		{"command begin", "item/started", `{"item":{"id":"i","type":"commandExecution","command":"pwd"}}`, map[string]any{"type": "exec_command_begin", "call_id": "i", "tool": "shell", "command": "pwd"}},
		{"command end", "item/completed", `{"item":{"id":"i","type":"commandExecution","exitCode":7}}`, map[string]any{"type": "exec_command_end", "call_id": "i", "tool": "shell", "exit_code": 7}},
		{"command no exit code", "item/completed", `{"item":{"id":"i","type":"commandExecution","exitCode":null}}`, map[string]any{"type": "exec_command_end", "call_id": "i", "tool": "shell"}},
		{"patch begin", "item/started", `{"item":{"id":"i","type":"fileChange","changes":[{"path":"/a"},{"path":"/b"}]}}`, map[string]any{"type": "exec_command_begin", "call_id": "i", "tool": "apply_patch", "paths": []string{"/a", "/b"}}},
		{"patch end", "item/completed", `{"item":{"id":"i","type":"fileChange","changes":[{"path":"/a"}]}}`, map[string]any{"type": "exec_command_end", "call_id": "i", "tool": "apply_patch", "paths": []string{"/a"}}},
		{"usage from last", "thread/tokenUsage/updated", `{"tokenUsage":{"last":{"inputTokens":10,"outputTokens":3,"totalTokens":13},"total":{"inputTokens":100,"outputTokens":30,"totalTokens":130}}}`, map[string]any{"type": "token_count", "prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}},
		{"error", "error", `{"error":{"message":"quota"},"willRetry":false}`, map[string]any{"type": "error", "message": "quota"}},
		{"completed", "turn/completed", `{"turn":{"status":"completed"}}`, map[string]any{"type": "task_complete"}},
		{"agent begin", "item/started", `{"item":{"type":"agentMessage"}}`, nil},
		{"reasoning begin", "item/started", `{"item":{"type":"reasoning"}}`, nil},
		{"reasoning end", "item/completed", `{"item":{"type":"reasoning"}}`, nil},
		{"unknown item", "item/completed", `{"item":{"type":"futureItem"}}`, nil},
		{"global", "mcpServer/startupStatus/updated", `{}`, nil},
		{"retryable error", "error", `{"error":{"message":"retrying"},"willRetry":true}`, nil},
		{"unknown method", "future/event", `{}`, nil},
		{"malformed", "item/completed", `{`, nil},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, ok := mapNotification(tc.method, json.RawMessage(tc.params))
			if ok != (tc.want != nil) || !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("mapNotification = %#v, %v; want %#v, %v", got, ok, tc.want, tc.want != nil)
			}
		})
	}
}
