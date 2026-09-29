package agent_test

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/agent"
	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/provider/chatcompletions"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

type recEmitter struct {
	mu     sync.Mutex
	events []map[string]any
}

func (e *recEmitter) Emit(_ context.Context, _ string, msg map[string]any) {
	e.mu.Lock()
	defer e.mu.Unlock()

	event := make(map[string]any, len(msg))
	for key, value := range msg {
		event[key] = value
	}
	e.events = append(e.events, event)
}

func (e *recEmitter) recordedEvents() []map[string]any {
	e.mu.Lock()
	defer e.mu.Unlock()

	events := make([]map[string]any, len(e.events))
	copy(events, e.events)
	return events
}

type stubApprover bool

func (a stubApprover) Approve(context.Context, string, agent.ApprovalRequest) bool {
	return bool(a)
}

func newRig(
	t *testing.T,
	turns []testutil.FakeTurn,
	o agent.Options,
	approve bool,
) (*agent.Runner, *agent.Session, *recEmitter, *testutil.FakeChat) {
	t.Helper()

	fake := testutil.NewFakeChat(t, turns)
	p, err := chatcompletions.New(
		"deepseek",
		config.Provider{API: config.APIChatCompletions, BaseURL: fake.URL},
		"test-key",
	)
	if err != nil {
		t.Fatalf("build chat-completions provider: %v", err)
	}
	adapter, ok := p.(*chatcompletions.Adapter)
	if !ok {
		t.Fatalf("chat-completions provider type = %T, want *chatcompletions.Adapter", p)
	}
	adapter.SetBackoff(func(int) time.Duration { return 0 })
	if o.Cwd == "" {
		o.Cwd = t.TempDir()
	}
	o.Provider = p
	session := agent.NewManager().Create(o)
	emitter := &recEmitter{}
	runner := &agent.Runner{Emitter: emitter, Approver: stubApprover(approve)}
	return runner, session, emitter, fake
}

func TestAgentLoopNormalCompletion(t *testing.T) {
	cwd := t.TempDir()
	turns := []testutil.FakeTurn{
		{ToolCalls: []testutil.FakeToolCall{{
			ID:   "call-1",
			Name: "shell",
			Args: `{"command":"cat hello.txt"}`,
		}}},
		{Text: "done"},
	}
	runner, session, emitter, fake := newRig(t, turns, agent.Options{
		Cwd:      cwd,
		Sandbox:  policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"),
	}, true)

	if err := os.WriteFile(filepath.Join(cwd, "hello.txt"), []byte("hello-integration"), 0o644); err != nil {
		t.Fatalf("write hello.txt: %v", err)
	}

	got, err := runner.Run(context.Background(), session, "read hello.txt")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "done" {
		t.Fatalf("Run() result = %q, want %q", got, "done")
	}
	if got := fake.RequestCount(); got != 2 {
		t.Fatalf("request count = %d, want 2", got)
	}

	toolMessage := lastRequestMessage(t, fake, 1)
	if toolMessage["role"] != "tool" {
		t.Fatalf("last message role = %#v, want tool", toolMessage["role"])
	}
	content, ok := toolMessage["content"].(string)
	if !ok || !strings.Contains(content, "hello-integration") {
		t.Fatalf("last tool message content = %#v, want it to contain hello-integration", toolMessage["content"])
	}

	events := emitter.recordedEvents()
	types := eventTypes(t, events)
	if len(types) == 0 || types[0] != "task_started" {
		t.Fatalf("event types = %v, want task_started first", types)
	}
	if types[len(types)-1] != "task_complete" {
		t.Fatalf("event types = %v, want task_complete last", types)
	}
	for _, want := range []string{"exec_command_begin", "exec_command_end"} {
		if !containsType(types, want) {
			t.Fatalf("event types = %v, want %s", types, want)
		}
	}
	if !hasTokenCount(events, 12) {
		t.Fatalf("events = %#v, want token_count with total_tokens 12", events)
	}
}

func TestAgentLoopWriteFileExecution(t *testing.T) {
	const (
		callID       = "call-write"
		relativePath = "nested/output.txt"
		content      = "written by the agent\n"
	)

	cwd := t.TempDir()
	turns := []testutil.FakeTurn{
		{ToolCalls: []testutil.FakeToolCall{{
			ID:   callID,
			Name: "write_file",
			Args: `{"path":"nested/output.txt","content":"written by the agent\n"}`,
		}}},
		{Text: "done"},
	}
	runner, session, emitter, fake := newRig(t, turns, agent.Options{
		Cwd:      cwd,
		Sandbox:  policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"),
	}, true)

	got, err := runner.Run(context.Background(), session, "write the output file")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "done" {
		t.Fatalf("Run() result = %q, want %q", got, "done")
	}

	written, err := os.ReadFile(filepath.Join(cwd, relativePath))
	if err != nil {
		t.Fatalf("read written file: %v", err)
	}
	if string(written) != content {
		t.Fatalf("written content = %q, want %q", written, content)
	}

	toolMessage := lastRequestMessage(t, fake, 1)
	if toolMessage["role"] != "tool" {
		t.Fatalf("last message role = %#v, want tool", toolMessage["role"])
	}
	wantToolContent := fmt.Sprintf("wrote %d bytes to %s", len(content), relativePath)
	if toolMessage["content"] != wantToolContent {
		t.Fatalf("last tool message content = %#v, want %q", toolMessage["content"], wantToolContent)
	}

	events := emitter.recordedEvents()
	beginIndex, endIndex := -1, -1
	for i, event := range events {
		switch event["type"] {
		case "exec_command_begin":
			if event["call_id"] == callID {
				beginIndex = i
				if event["tool"] != "write_file" || event["path"] != relativePath {
					t.Fatalf("begin event = %#v", event)
				}
			}
		case "exec_command_end":
			if event["call_id"] == callID {
				endIndex = i
				if event["tool"] != "write_file" {
					t.Fatalf("end event = %#v", event)
				}
				if _, ok := event["error"]; ok {
					t.Fatalf("successful write_file end event unexpectedly contains error: %#v", event)
				}
			}
		}
	}
	if beginIndex < 0 || endIndex != beginIndex+1 {
		t.Fatalf("event types = %v, want adjacent write_file begin/end pair", eventTypes(t, events))
	}
}

func TestAgentLoopSandboxDenial(t *testing.T) {
	cwd := t.TempDir()
	turns := []testutil.FakeTurn{
		{ToolCalls: []testutil.FakeToolCall{{
			ID:   "call-2",
			Name: "write_file",
			Args: `{"path":"blocked.txt","content":"nope"}`,
		}}},
		{Text: "ok"},
	}
	runner, session, emitter, fake := newRig(t, turns, agent.Options{
		Cwd:      cwd,
		Sandbox:  policy.Sandbox("read-only"),
		Approval: policy.ApprovalPolicy("never"),
	}, true)

	got, err := runner.Run(context.Background(), session, "write blocked.txt")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "ok" {
		t.Fatalf("Run() result = %q, want %q", got, "ok")
	}

	toolMessage := lastRequestMessage(t, fake, 1)
	if toolMessage["role"] != "tool" {
		t.Fatalf("last message role = %#v, want tool", toolMessage["role"])
	}
	content, ok := toolMessage["content"].(string)
	if !ok || !strings.Contains(content, "denied by sandbox policy") {
		t.Fatalf("last tool message content = %#v, want sandbox denial", toolMessage["content"])
	}
	if _, err := os.Stat(filepath.Join(cwd, "blocked.txt")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("blocked.txt exists or stat failed unexpectedly: %v", err)
	}
	assertNoExecEvents(t, emitter.recordedEvents())
}

func TestAgentLoopApprovalDenial(t *testing.T) {
	cwd := t.TempDir()
	turns := []testutil.FakeTurn{
		{ToolCalls: []testutil.FakeToolCall{{
			ID:   "call-3",
			Name: "shell",
			Args: `{"command":"touch x"}`,
		}}},
		{Text: "ok"},
	}
	runner, session, emitter, fake := newRig(t, turns, agent.Options{
		Cwd:      cwd,
		Sandbox:  policy.Sandbox("read-only"),
		Approval: policy.ApprovalPolicy("on-request"),
	}, false)

	got, err := runner.Run(context.Background(), session, "touch x")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "ok" {
		t.Fatalf("Run() result = %q, want %q", got, "ok")
	}

	toolMessage := lastRequestMessage(t, fake, 1)
	if toolMessage["role"] != "tool" {
		t.Fatalf("last message role = %#v, want tool", toolMessage["role"])
	}
	content, ok := toolMessage["content"].(string)
	if !ok || !strings.Contains(content, "approval was not granted") {
		t.Fatalf("last tool message content = %#v, want approval denial", toolMessage["content"])
	}
	if _, err := os.Stat(filepath.Join(cwd, "x")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("x exists or stat failed unexpectedly: %v", err)
	}
	assertNoExecEvents(t, emitter.recordedEvents())
}

func TestAgentLoopTurnLimitReachedThenResumed(t *testing.T) {
	turns := []testutil.FakeTurn{
		{ToolCalls: []testutil.FakeToolCall{{
			ID:   "call-4a",
			Name: "shell",
			Args: `{"command":"pwd"}`,
		}}},
		{ToolCalls: []testutil.FakeToolCall{{
			ID:   "call-4b",
			Name: "shell",
			Args: `{"command":"pwd"}`,
		}}},
		{Text: "late"},
	}
	runner, session, _, fake := newRig(t, turns, agent.Options{
		MaxTurns: 2,
		Sandbox:  policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"),
	}, true)

	got, err := runner.Run(context.Background(), session, "keep going")
	if got != "" {
		t.Fatalf("first Run() result = %q, want empty", got)
	}
	if err == nil || !strings.Contains(err.Error(), "turn limit reached (2)") {
		t.Fatalf("first Run() error = %v, want turn limit reached (2)", err)
	}
	if got := fake.RequestCount(); got != 2 {
		t.Fatalf("request count after first Run() = %d, want 2", got)
	}

	got, err = runner.Run(context.Background(), session, "continue")
	if err != nil {
		t.Fatalf("resumed Run() error = %v", err)
	}
	if got != "late" {
		t.Fatalf("resumed Run() result = %q, want %q", got, "late")
	}
	if got := fake.RequestCount(); got != 3 {
		t.Fatalf("request count after resumed Run() = %d, want 3", got)
	}
	assertRequestHistory(t, fake, 2, "keep going", "call-4a", "call-4b")
}

func TestAgentLoopAPIRetryRecovery(t *testing.T) {
	t.Run("retry then succeed", func(t *testing.T) {
		turns := []testutil.FakeTurn{
			{Status: 500},
			{Status: 429},
			{Text: "recovered"},
		}
		runner, session, _, fake := newRig(t, turns, agent.Options{}, true)

		got, err := runner.Run(context.Background(), session, "recover")
		if err != nil {
			t.Fatalf("Run() error = %v", err)
		}
		if got != "recovered" {
			t.Fatalf("Run() result = %q, want %q", got, "recovered")
		}
		if got := fake.RequestCount(); got != 3 {
			t.Fatalf("request count = %d, want 3", got)
		}
	})

	t.Run("retry exhausted then resumed", func(t *testing.T) {
		turns := []testutil.FakeTurn{
			{Status: 500},
			{Status: 500},
			{Status: 500},
			{Status: 500},
			{Text: "after"},
		}
		runner, session, emitter, fake := newRig(t, turns, agent.Options{}, true)

		firstGot, err := runner.Run(context.Background(), session, "first")
		if err == nil {
			t.Fatalf("first Run() = (%q, nil), want an error", firstGot)
		}
		if !strings.Contains(err.Error(), "status code: 500") {
			t.Fatalf("first Run() error = %q, want exhausted upstream 500 error", err)
		}
		if !containsType(eventTypes(t, emitter.recordedEvents()), "error") {
			t.Fatal("first Run() did not emit an error event")
		}
		if got := fake.RequestCount(); got != 4 {
			t.Fatalf("request count after first Run() = %d, want 4", got)
		}

		got, err := runner.Run(context.Background(), session, "second")
		if err != nil {
			t.Fatalf("resumed Run() error = %v", err)
		}
		if got != "after" {
			t.Fatalf("resumed Run() result = %q, want %q", got, "after")
		}
		if got := fake.RequestCount(); got != 5 {
			t.Fatalf("request count after resumed Run() = %d, want 5", got)
		}
		// The "first" prompt failed before any assistant message was
		// appended, so its Run restored the history to its entry length:
		// the resumed Run carries only its own prompt.
		assertRequestHistory(t, fake, 4, "second")
		for i, messageValue := range requestMessages(t, fake, 4) {
			message, ok := messageValue.(map[string]any)
			if !ok {
				t.Fatalf("request 4 message %d = %#v, want an object", i, messageValue)
			}
			if role, _ := message["role"].(string); role == "user" {
				if content, _ := message["content"].(string); content == "first" {
					t.Fatalf("request 4 replays the abandoned prompt %q", content)
				}
			}
		}
	})
}

func assertRequestHistory(
	t *testing.T,
	fake *testutil.FakeChat,
	requestIndex int,
	originalPrompt string,
	toolCallIDs ...string,
) {
	t.Helper()

	foundPrompt := false
	toolContent := make(map[string]string, len(toolCallIDs))
	for _, callID := range toolCallIDs {
		toolContent[callID] = ""
	}

	for i, messageValue := range requestMessages(t, fake, requestIndex) {
		message, ok := messageValue.(map[string]any)
		if !ok {
			t.Fatalf("request %d message %d = %#v, want an object", requestIndex, i, messageValue)
		}
		role, _ := message["role"].(string)
		content, _ := message["content"].(string)
		if role == "user" && content == originalPrompt {
			foundPrompt = true
		}
		if role == "tool" {
			callID, _ := message["tool_call_id"].(string)
			if _, wanted := toolContent[callID]; wanted {
				toolContent[callID] = content
			}
		}
	}

	if !foundPrompt {
		t.Fatalf("request %d does not contain original user prompt %q", requestIndex, originalPrompt)
	}
	for callID, content := range toolContent {
		if content == "" {
			t.Fatalf("request %d does not contain a non-empty tool message for %s", requestIndex, callID)
		}
	}
}

func lastRequestMessage(t *testing.T, fake *testutil.FakeChat, requestIndex int) map[string]any {
	t.Helper()

	messages := requestMessages(t, fake, requestIndex)
	message, ok := messages[len(messages)-1].(map[string]any)
	if !ok {
		t.Fatalf("request %d last message = %#v, want an object", requestIndex, messages[len(messages)-1])
	}
	return message
}

func requestMessages(t *testing.T, fake *testutil.FakeChat, requestIndex int) []any {
	t.Helper()

	request := fake.Request(requestIndex)
	messages, ok := request["messages"].([]any)
	if !ok || len(messages) == 0 {
		t.Fatalf("request %d messages = %#v, want a non-empty array", requestIndex, request["messages"])
	}
	return messages
}

func eventTypes(t *testing.T, events []map[string]any) []string {
	t.Helper()

	types := make([]string, len(events))
	for i, event := range events {
		eventType, ok := event["type"].(string)
		if !ok {
			t.Fatalf("event %d type = %#v, want a string", i, event["type"])
		}
		types[i] = eventType
	}
	return types
}

func containsType(types []string, want string) bool {
	for _, eventType := range types {
		if eventType == want {
			return true
		}
	}
	return false
}

func hasTokenCount(events []map[string]any, wantTotal int) bool {
	for _, event := range events {
		if event["type"] == "token_count" && event["total_tokens"] == wantTotal {
			return true
		}
	}
	return false
}

func assertNoExecEvents(t *testing.T, events []map[string]any) {
	t.Helper()

	for _, eventType := range eventTypes(t, events) {
		if eventType == "exec_command_begin" || eventType == "exec_command_end" {
			t.Fatalf("denied call emitted %s", eventType)
		}
	}
}
