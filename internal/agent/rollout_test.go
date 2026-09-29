package agent

import (
	"bufio"
	"context"
	"encoding/json"
	"os"
	"reflect"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/rollout"
)

type rolloutLine struct {
	Type    string         `json:"type"`
	Payload map[string]any `json:"payload"`
}

func readRollout(t *testing.T, path string) []rolloutLine {
	t.Helper()
	file, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	var lines []rolloutLine
	scanner := bufio.NewScanner(file)
	scanner.Buffer(make([]byte, 1024*1024), 1024*1024)
	for scanner.Scan() {
		var line rolloutLine
		if err := json.Unmarshal(scanner.Bytes(), &line); err != nil {
			t.Fatalf("invalid line %q: %v", scanner.Text(), err)
		}
		lines = append(lines, line)
	}
	return lines
}

func lineKinds(lines []rolloutLine) []string {
	kinds := make([]string, len(lines))
	for i, line := range lines {
		kind := line.Type
		if sub, ok := line.Payload["type"].(string); ok && line.Type != "turn_context" {
			kind += ":" + sub
		}
		kinds[i] = kind
	}
	return kinds
}

func TestRunnerWritesRollout(t *testing.T) {
	t.Setenv("CODEX_HOME", t.TempDir())
	t.Setenv("SUBAGENT_MCP_ROLLOUT", "")

	shell := toolCall("call-shell", "shell", `{"command":"echo hi"}`)
	patchArgs, _ := json.Marshal(map[string]string{"patch": "*** Begin Patch\n*** Add File: n.txt\n+x\n*** End Patch"})
	applyPatch := toolCall("call-patch", "apply_patch", string(patchArgs))
	usage := &provider.Usage{
		Input: 10, Output: 4, Total: 14,
		Cached: 6, Reasoning: 2,
	}
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{Reasoning: "plan", Text: "working", ToolCalls: []provider.ToolCall{shell, applyPatch}, Usage: usage}},
		{result: &provider.TurnResult{Text: "all done", Usage: usage}},
	}}
	session := newTestSession(t, Options{Provider: client, Sandbox: "danger-full-access", ReasoningEffort: "xhigh", EffortSent: "max"})
	recorder := rollout.Open(session.ID, time.Now())
	session.AttachRollout(recorder)
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

	if _, err := runner.Run(context.Background(), session, "do it"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}

	lines := readRollout(t, recorder.Path())
	want := []string{
		"turn_context",
		"event_msg:task_started",
		"response_item:message",
		"event_msg:user_message",
		"event_msg:provider_request",
		"response_item:reasoning",
		"event_msg:token_count",
		"response_item:message",
		"event_msg:agent_message",
		"response_item:function_call",
		"response_item:function_call_output",
		"response_item:custom_tool_call",
		"event_msg:patch_apply_end",
		"response_item:custom_tool_call_output",
		"event_msg:provider_request",
		"event_msg:token_count",
		"response_item:message",
		"event_msg:agent_message",
		"event_msg:task_complete",
	}
	if got := lineKinds(lines); !reflect.DeepEqual(got, want) {
		t.Fatalf("rollout kinds =\n%v\nwant\n%v", got, want)
	}

	if lines[0].Payload["effort"] != "xhigh" || lines[0].Payload["effort_sent"] != "max" ||
		lines[0].Payload["model"] != "deepseek-v4-pro" {
		t.Fatalf("turn_context = %#v", lines[0].Payload)
	}
	info := lines[15].Payload["info"].(map[string]any)
	total := info["total_token_usage"].(map[string]any)
	last := info["last_token_usage"].(map[string]any)
	if total["input_tokens"] != float64(20) || total["cached_input_tokens"] != float64(12) ||
		total["reasoning_output_tokens"] != float64(4) || last["total_tokens"] != float64(14) {
		t.Fatalf("token_count info = %#v", info)
	}
	if lines[12].Payload["success"] != true {
		t.Fatalf("patch_apply_end = %#v", lines[12].Payload)
	}
	if lines[18].Payload["last_agent_message"] != "all done" {
		t.Fatalf("task_complete = %#v", lines[18].Payload)
	}
	request := lines[4].Payload
	if request["provider"] != "stub" || request["model"] != session.model {
		t.Fatalf("provider_request = %#v", request)
	}
	if _, ok := request["duration_ms"].(float64); !ok {
		t.Fatalf("duration_ms = %#v (%T), want numeric JSON value", request["duration_ms"], request["duration_ms"])
	}
}

func TestRunnerRolloutRecordsError(t *testing.T) {
	t.Setenv("CODEX_HOME", t.TempDir())
	t.Setenv("SUBAGENT_MCP_ROLLOUT", "")
	client := &stubProvider{}
	session := newTestSession(t, Options{Provider: client})
	recorder := rollout.Open(session.ID, time.Now())
	session.AttachRollout(recorder)
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

	if _, err := runner.Run(context.Background(), session, "fail"); err == nil {
		t.Fatalf("Run() error = nil, want error")
	}
	lines := readRollout(t, recorder.Path())
	if last := lines[len(lines)-1]; last.Type != "event_msg" || last.Payload["type"] != "error" {
		t.Fatalf("last line = %#v", last)
	}
}
