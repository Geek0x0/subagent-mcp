package agent

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
)

type stubTurn struct {
	result *provider.TurnResult
	err    error
	deltas []string
}

type stubProvider struct {
	mu       sync.Mutex
	turns    []stubTurn
	requests []provider.TurnRequest
	block    <-chan struct{}
	entered  chan<- struct{}
}

func (p *stubProvider) Name() string { return "stub" }

func (p *stubProvider) Turn(ctx context.Context, req provider.TurnRequest, onDelta func(string)) (*provider.TurnResult, error) {
	p.mu.Lock()
	copied := req
	copied.Messages = append([]provider.Message(nil), req.Messages...)
	copied.Tools = append([]provider.ToolSpec(nil), req.Tools...)
	p.requests = append(p.requests, copied)
	if len(p.turns) == 0 {
		p.mu.Unlock()
		return nil, errors.New("stub provider has no queued turn")
	}
	turn := p.turns[0]
	p.turns = p.turns[1:]
	block, entered := p.block, p.entered
	p.mu.Unlock()
	if entered != nil {
		select {
		case entered <- struct{}{}:
		default:
		}
	}
	if block != nil {
		select {
		case <-block:
		case <-ctx.Done():
			return nil, ctx.Err()
		}
	}
	for _, delta := range turn.deltas {
		onDelta(delta)
	}
	return turn.result, turn.err
}

func (p *stubProvider) recordedRequests() []provider.TurnRequest {
	p.mu.Lock()
	defer p.mu.Unlock()
	return append([]provider.TurnRequest(nil), p.requests...)
}

type recEmitter struct {
	mu     sync.Mutex
	events []map[string]any
}

type panicOnceEmitter struct {
	recorder  *recEmitter
	panicType string
	once      sync.Once
}

func (e *panicOnceEmitter) Emit(ctx context.Context, threadID string, msg map[string]any) {
	e.recorder.Emit(ctx, threadID, msg)
	if msg["type"] != e.panicType {
		return
	}

	shouldPanic := false
	e.once.Do(func() { shouldPanic = true })
	if shouldPanic {
		panic("emitter boom")
	}
}

func (e *recEmitter) Emit(_ context.Context, _ string, msg map[string]any) {
	e.mu.Lock()
	defer e.mu.Unlock()

	copyOfMessage := make(map[string]any, len(msg))
	for key, value := range msg {
		copyOfMessage[key] = value
	}
	e.events = append(e.events, copyOfMessage)
}

func (e *recEmitter) recordedEvents() []map[string]any {
	e.mu.Lock()
	defer e.mu.Unlock()

	events := make([]map[string]any, len(e.events))
	for i, event := range e.events {
		events[i] = make(map[string]any, len(event))
		for key, value := range event {
			events[i][key] = value
		}
	}
	return events
}

type stubApprover struct {
	mu       sync.Mutex
	approved bool
	requests []ApprovalRequest
}

func (a *stubApprover) Approve(_ context.Context, _ string, req ApprovalRequest) bool {
	a.mu.Lock()
	defer a.mu.Unlock()

	a.requests = append(a.requests, req)
	return a.approved
}

func (a *stubApprover) recordedRequests() []ApprovalRequest {
	a.mu.Lock()
	defer a.mu.Unlock()

	return append([]ApprovalRequest(nil), a.requests...)
}

type stubThread struct {
	run   func(context.Context, string, string, provider.ThreadCallbacks) (string, error)
	close func()
}

func (s *stubThread) Run(ctx context.Context, prompt, effort string, cb provider.ThreadCallbacks) (string, error) {
	return s.run(ctx, prompt, effort, cb)
}

func (s *stubThread) Close() {
	if s.close != nil {
		s.close()
	}
}

type emitterFunc func(context.Context, string, map[string]any)

func (f emitterFunc) Emit(ctx context.Context, threadID string, event map[string]any) {
	f(ctx, threadID, event)
}

type approverFunc func(context.Context, string, ApprovalRequest) bool

func (f approverFunc) Approve(ctx context.Context, threadID string, req ApprovalRequest) bool {
	return f(ctx, threadID, req)
}

func TestRunnerDelegatesToThread(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	var gotPrompt, gotEffort string
	thread := &stubThread{run: func(gotCtx context.Context, prompt, effort string, cb provider.ThreadCallbacks) (string, error) {
		if gotCtx != ctx {
			t.Error("Thread.Run() received a different context")
		}
		gotPrompt, gotEffort = prompt, effort
		cb.Emit(map[string]any{"type": "agent_message", "message": "hi"})
		if !cb.Approve(provider.ApprovalRequest{Tool: "shell", Command: "ls"}) {
			t.Error("approval result = false, want true")
		}
		return "done", nil
	}}
	session := newTestSession(t, Options{Thread: thread, ReasoningEffort: "xhigh", EffortSent: "max"})
	emitter := &recEmitter{}
	var approvals []ApprovalRequest
	runner := &Runner{
		Emitter: emitterFunc(func(gotCtx context.Context, threadID string, event map[string]any) {
			if gotCtx != ctx || threadID != session.ID {
				t.Errorf("Emit() context/thread = (%v, %q), want (%v, %q)", gotCtx, threadID, ctx, session.ID)
			}
			emitter.Emit(gotCtx, threadID, event)
		}),
		Approver: approverFunc(func(gotCtx context.Context, threadID string, req ApprovalRequest) bool {
			if gotCtx != ctx || threadID != session.ID {
				t.Errorf("Approve() context/thread = (%v, %q), want (%v, %q)", gotCtx, threadID, ctx, session.ID)
			}
			approvals = append(approvals, req)
			return true
		}),
	}

	before := time.Now()
	got, err := runner.Run(ctx, session, "finish the task")
	if err != nil || got != "done" {
		t.Fatalf("Run() = (%q, %v), want (done, nil)", got, err)
	}
	if gotPrompt != "finish the task" || gotEffort != "max" {
		t.Fatalf("Thread.Run() prompt/effort = (%q, %q)", gotPrompt, gotEffort)
	}
	wantEvents := []map[string]any{{"type": "agent_message", "message": "hi"}}
	if got := emitter.recordedEvents(); !reflect.DeepEqual(got, wantEvents) {
		t.Fatalf("events = %#v, want %#v", got, wantEvents)
	}
	if want := []ApprovalRequest{{Tool: "shell", Command: "ls"}}; !reflect.DeepEqual(approvals, want) {
		t.Fatalf("approvals = %#v, want %#v", approvals, want)
	}
	if !session.lastUsed.After(before) {
		t.Fatalf("session.lastUsed = %v, want after %v", session.lastUsed, before)
	}
	if len(session.messages) != 0 || session.turnID != "" || session.system != "" {
		t.Fatalf("thread session initialized native state: messages=%#v, turnID=%q, system=%q", session.messages, session.turnID, session.system)
	}
}

func TestRunnerThreadErrorEmitsError(t *testing.T) {
	boom := errors.New("boom")
	thread := &stubThread{run: func(context.Context, string, string, provider.ThreadCallbacks) (string, error) {
		return "", boom
	}}
	session := newTestSession(t, Options{Thread: thread})
	emitter := &recEmitter{}
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}

	before := time.Now()
	if got, err := runner.Run(context.Background(), session, "hello"); got != "" || !errors.Is(err, boom) {
		t.Fatalf("Run() = (%q, %v), want (empty, boom)", got, err)
	}
	wantEvents := []map[string]any{{"type": "error", "message": "boom"}}
	if got := emitter.recordedEvents(); !reflect.DeepEqual(got, wantEvents) {
		t.Fatalf("events = %#v, want %#v", got, wantEvents)
	}
	if !session.lastUsed.After(before) {
		t.Fatalf("session.lastUsed = %v, want after %v", session.lastUsed, before)
	}
	thread.run = func(context.Context, string, string, provider.ThreadCallbacks) (string, error) {
		return "recovered", nil
	}
	if got, err := runner.Run(context.Background(), session, "retry"); got != "recovered" || err != nil {
		t.Fatalf("Run() after error = (%q, %v), want (recovered, nil)", got, err)
	}
}

func TestRunnerThreadBusy(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	unblock := make(chan struct{})
	entered := make(chan struct{}, 1)
	thread := &stubThread{run: func(ctx context.Context, _, _ string, _ provider.ThreadCallbacks) (string, error) {
		entered <- struct{}{}
		select {
		case <-unblock:
			return "done", nil
		case <-ctx.Done():
			return "", ctx.Err()
		}
	}}
	session := newTestSession(t, Options{Thread: thread})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	firstResult := make(chan error, 1)
	go func() {
		_, err := runner.Run(ctx, session, "first")
		firstResult <- err
	}()
	select {
	case <-entered:
	case <-time.After(2 * time.Second):
		t.Fatal("first Run() did not reach the blocked thread")
	}

	secondResult := make(chan error, 1)
	go func() {
		_, err := runner.Run(ctx, session, "second")
		secondResult <- err
	}()
	select {
	case err := <-secondResult:
		if !errors.Is(err, ErrBusy) {
			t.Fatalf("concurrent Run() error = %v, want ErrBusy", err)
		}
	case <-time.After(time.Second):
		t.Fatal("concurrent Run() blocked instead of returning ErrBusy")
	}

	close(unblock)
	select {
	case err := <-firstResult:
		if err != nil {
			t.Fatalf("first Run() error = %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("first Run() did not finish after unblocking the thread")
	}
}

func TestManagerEvictionClosesThread(t *testing.T) {
	for _, eviction := range []string{"idle TTL", "capacity"} {
		t.Run(eviction, func(t *testing.T) {
			manager := NewManager()
			current := time.Now()
			manager.now = func() time.Time { return current }
			closed := 0
			thread := &stubThread{close: func() {
				closed++
				if !manager.mu.TryLock() {
					t.Error("Thread.Close() called with manager lock held")
					return
				}
				manager.mu.Unlock()
			}}
			oldest := manager.Create(Options{Thread: thread})
			if eviction == "idle TTL" {
				current = current.Add(sessionIdleTTL + time.Second)
			} else {
				for i := 1; i < maxSessions; i++ {
					current = current.Add(time.Minute)
					manager.Create(Options{})
				}
			}
			fresh := manager.Create(Options{})
			if got, ok := manager.Get(oldest.ID); ok || got != nil {
				t.Fatalf("Get(oldest.ID) = (%#v, %v), want (nil, false)", got, ok)
			}
			if got, ok := manager.Get(fresh.ID); !ok || got != fresh {
				t.Fatalf("Get(fresh.ID) = (%#v, %v), want fresh session", got, ok)
			}
			if closed != 1 {
				t.Fatalf("Close() calls = %d, want 1", closed)
			}
			manager.Create(Options{})
			if closed != 1 {
				t.Fatalf("Close() calls after another Create() = %d, want 1", closed)
			}
		})
	}
}

func TestRunnerPureTextOneTurn(t *testing.T) {
	client := &stubProvider{turns: []stubTurn{{result: &provider.TurnResult{Text: "done"}}}}
	emitter := &recEmitter{}
	session := newTestSession(t, Options{Provider: client})
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}

	got, err := runner.Run(context.Background(), session, "finish the task")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "done" {
		t.Fatalf("Run() result = %q, want %q", got, "done")
	}
	if gotTypes := eventTypes(t, emitter.recordedEvents()); !reflect.DeepEqual(gotTypes, []string{
		"task_started",
		"agent_message",
		"task_complete",
	}) {
		t.Fatalf("event types = %v", gotTypes)
	}
	lastMessage := session.messages[len(session.messages)-1]
	if lastMessage.Role != provider.RoleAssistant || lastMessage.Text != "done" {
		t.Fatalf("last message = %#v", lastMessage)
	}
}

func TestRunnerIncludesReasoningEffort(t *testing.T) {
	tests := []struct {
		name    string
		options Options
		want    string
	}{
		{name: "default", options: Options{}, want: "high"},
		{name: "explicit override", options: Options{ReasoningEffort: "low"}, want: "low"},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			client := &stubProvider{turns: []stubTurn{{result: &provider.TurnResult{Text: "done"}}}}
			test.options.Provider = client
			session := newTestSession(t, test.options)
			runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

			if _, err := runner.Run(context.Background(), session, "finish the task"); err != nil {
				t.Fatalf("Run() error = %v", err)
			}

			requests := client.recordedRequests()
			if len(requests) != 1 {
				t.Fatalf("request count = %d, want 1", len(requests))
			}
			if requests[0].Effort != test.want {
				t.Fatalf("request reasoning effort = %q, want %q", requests[0].Effort, test.want)
			}
		})
	}
}

func TestRunnerShellToolCallThenText(t *testing.T) {
	call := toolCall("call-shell", "shell", `{"command":"echo hi"}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
		{result: &provider.TurnResult{Text: "ok"}},
	}}
	emitter := &recEmitter{}
	session := newTestSession(t, Options{
		Provider: client,
		Cwd:      t.TempDir(),
		Sandbox:  policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"),
	})
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}

	got, err := runner.Run(context.Background(), session, "say hi")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "ok" {
		t.Fatalf("Run() result = %q, want %q", got, "ok")
	}

	requests := client.recordedRequests()
	if len(requests) != 2 {
		t.Fatalf("request count = %d, want 2", len(requests))
	}
	toolMessage := findToolMessage(t, requests[1].Messages, call.ID)
	if !strings.Contains(toolMessage.Text, "hi") {
		t.Fatalf("tool result = %q, want it to contain hi", toolMessage.Text)
	}

	events := emitter.recordedEvents()
	if gotTypes := eventTypes(t, events); !reflect.DeepEqual(gotTypes, []string{
		"task_started",
		"exec_command_begin",
		"exec_command_end",
		"agent_message",
		"task_complete",
	}) {
		t.Fatalf("event types = %v", gotTypes)
	}
	begin := events[1]
	if begin["call_id"] != call.ID || begin["tool"] != "shell" || begin["command"] != "echo hi" {
		t.Fatalf("begin event = %#v", begin)
	}
	end := events[2]
	if end["call_id"] != call.ID || end["tool"] != "shell" || end["exit_code"] != 0 {
		t.Fatalf("end event = %#v", end)
	}
	if _, ok := end["error"]; ok {
		t.Fatalf("successful shell end event unexpectedly contains error: %#v", end)
	}
}

func TestRunnerPassesReasoningBackInHistory(t *testing.T) {
	call := toolCall("call-1", "shell", `{"command":"echo hi"}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{
			Reasoning: "plan A",
			ToolCalls: []provider.ToolCall{call},
			Opaque:    json.RawMessage(`{"r":"plan A"}`),
		}},
		{result: &provider.TurnResult{Text: "done"}},
	}}
	session := newTestSession(t, Options{Provider: client, Sandbox: "danger-full-access", Approval: "never"})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

	if _, err := runner.Run(context.Background(), session, "use a tool"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}

	requests := client.recordedRequests()
	if len(requests) != 2 {
		t.Fatalf("request count = %d, want 2", len(requests))
	}

	var assistant *provider.Message
	for i := range requests[1].Messages {
		message := &requests[1].Messages[i]
		if message.Role == provider.RoleAssistant && len(message.ToolCalls) > 0 {
			assistant = message
			break
		}
	}
	if assistant == nil {
		t.Fatalf("no assistant message carrying a tool call in %#v", requests[1].Messages)
	}
	if string(assistant.Opaque) != `{"r":"plan A"}` {
		t.Fatalf("assistant Opaque = %s, want %s", assistant.Opaque, `{"r":"plan A"}`)
	}
}

func TestRunnerEmptyFileResultHasNonEmptyToolContent(t *testing.T) {
	cwd := t.TempDir()
	if err := os.WriteFile(filepath.Join(cwd, "empty.txt"), nil, 0o644); err != nil {
		t.Fatalf("create empty file: %v", err)
	}

	call := toolCall("call-empty", "read_file", `{"path":"empty.txt"}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
		{result: &provider.TurnResult{Text: "empty file handled"}},
	}}
	session := newTestSession(t, Options{
		Provider: client,
		Cwd:      cwd,
		Sandbox:  policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"),
	})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

	if _, err := runner.Run(context.Background(), session, "read empty.txt"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	requests := client.recordedRequests()
	toolMessage := findToolMessage(t, requests[1].Messages, call.ID)
	if toolMessage.Text == "" {
		t.Fatal("empty file produced a tool message with empty Text")
	}
	if toolMessage.Text != "(empty output)" {
		t.Fatalf("empty file tool result = %q, want %q", toolMessage.Text, "(empty output)")
	}
}

func TestRunnerApprovalDenied(t *testing.T) {
	const justification = "the requested change needs a file write"
	call := toolCall("call-write", "write_file", `{"path":"out.txt","content":"hello","justification":"`+justification+`"}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
		{result: &provider.TurnResult{Text: "blocked but finished"}},
	}}
	emitter := &recEmitter{}
	approver := &stubApprover{approved: false}
	session := newTestSession(t, Options{
		Provider: client,
		Cwd:      t.TempDir(),
		Sandbox:  policy.Sandbox("read-only"),
		Approval: policy.ApprovalPolicy("on-request"),
	})
	runner := &Runner{Emitter: emitter, Approver: approver}

	got, err := runner.Run(context.Background(), session, "write a file")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "blocked but finished" {
		t.Fatalf("Run() result = %q", got)
	}
	requests := client.recordedRequests()
	toolMessage := findToolMessage(t, requests[1].Messages, call.ID)
	if !strings.Contains(toolMessage.Text, "approval was not granted") {
		t.Fatalf("tool result = %q", toolMessage.Text)
	}
	if !toolMessage.IsError {
		t.Fatalf("approval denial tool message = %#v, want IsError", toolMessage)
	}
	assertNoExecEvents(t, emitter.recordedEvents())

	approvalRequests := approver.recordedRequests()
	if len(approvalRequests) != 1 {
		t.Fatalf("approval request count = %d, want 1", len(approvalRequests))
	}
	if !strings.Contains(approvalRequests[0].Reason, "read-only sandbox") ||
		!strings.Contains(approvalRequests[0].Reason, justification) {
		t.Fatalf("approval reason = %q, want policy reason and justification %q", approvalRequests[0].Reason, justification)
	}
	if approvalRequests[0].Tool != "write_file" || approvalRequests[0].Path != "out.txt" {
		t.Fatalf("approval request = %#v", approvalRequests[0])
	}
}

func TestRunnerRecoversToolExecutionPanicAndCompletesHistory(t *testing.T) {
	call := toolCall("call-panic", "shell", `{"command":"echo should-not-run"}`)
	afterPanicCall := toolCall("call-after-panic", "shell", `{"command":"echo after-panic"}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call, afterPanicCall}}},
		{result: &provider.TurnResult{Text: "recovered"}},
	}}
	recorder := &recEmitter{}
	emitter := &panicOnceEmitter{recorder: recorder, panicType: "exec_command_begin"}
	session := newTestSession(t, Options{
		Provider: client,
		Cwd:      t.TempDir(),
		Sandbox:  policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"),
	})
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}

	got, err := runner.Run(context.Background(), session, "trigger a tool panic")
	if err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if got != "recovered" {
		t.Fatalf("Run() result = %q, want %q", got, "recovered")
	}

	requests := client.recordedRequests()
	if len(requests) != 2 {
		t.Fatalf("request count = %d, want 2", len(requests))
	}
	toolMessage := findToolMessage(t, requests[1].Messages, call.ID)
	if !strings.Contains(toolMessage.Text, "tool execution panicked") ||
		!strings.Contains(toolMessage.Text, "emitter boom") {
		t.Fatalf("panic tool result = %q", toolMessage.Text)
	}
	if !toolMessage.IsError {
		t.Fatalf("panic tool message = %#v, want IsError", toolMessage)
	}
	afterPanicMessage := findToolMessage(t, requests[1].Messages, afterPanicCall.ID)
	if !strings.Contains(afterPanicMessage.Text, "after-panic") {
		t.Fatalf("post-panic tool result = %q", afterPanicMessage.Text)
	}
	assertCompleteToolHistory(t, requests[1].Messages)

	events := recorder.recordedEvents()
	if gotTypes := eventTypes(t, events); !reflect.DeepEqual(gotTypes, []string{
		"task_started",
		"exec_command_begin",
		"exec_command_end",
		"exec_command_begin",
		"exec_command_end",
		"agent_message",
		"task_complete",
	}) {
		t.Fatalf("event types = %v", gotTypes)
	}
	end := events[2]
	if end["call_id"] != call.ID || end["tool"] != "shell" || end["exit_code"] != -1 {
		t.Fatalf("panic end event = %#v", end)
	}
	if _, ok := end["error"]; ok {
		t.Fatalf("panic end event unexpectedly contains error: %#v", end)
	}
}

func TestRunnerNeverPolicyDenial(t *testing.T) {
	call := toolCall("call-rm", "shell", `{"command":"rm x"}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
		{result: &provider.TurnResult{Text: "not removed"}},
	}}
	emitter := &recEmitter{}
	session := newTestSession(t, Options{
		Provider: client,
		Cwd:      t.TempDir(),
		Sandbox:  policy.Sandbox("read-only"),
		Approval: policy.ApprovalPolicy("never"),
	})
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{approved: true}}

	if _, err := runner.Run(context.Background(), session, "remove x"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	requests := client.recordedRequests()
	toolMessage := findToolMessage(t, requests[1].Messages, call.ID)
	if !strings.Contains(toolMessage.Text, "denied by sandbox policy") {
		t.Fatalf("tool result = %q", toolMessage.Text)
	}
	if !toolMessage.IsError {
		t.Fatalf("policy denial tool message = %#v, want IsError", toolMessage)
	}
	assertNoExecEvents(t, emitter.recordedEvents())
}

func TestRunnerTurnLimitReachedThenResumed(t *testing.T) {
	firstCall := toolCall("call-one", "shell", `{"command":"echo one"}`)
	secondCall := toolCall("call-two", "shell", `{"command":"echo two"}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{firstCall}}},
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{secondCall}}},
		{result: &provider.TurnResult{Text: "resumed"}},
	}}
	emitter := &recEmitter{}
	session := newTestSession(t, Options{
		Provider: client,
		Cwd:      t.TempDir(),
		Sandbox:  policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"),
		MaxTurns: 2,
	})
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}

	got, err := runner.Run(context.Background(), session, "keep using tools")
	if got != "" {
		t.Fatalf("first Run() result = %q, want empty", got)
	}
	if err == nil || !strings.Contains(err.Error(), "turn limit reached (2)") {
		t.Fatalf("first Run() error = %v", err)
	}
	if !containsEventType(t, emitter.recordedEvents(), "error") {
		t.Fatal("turn-limit run did not emit an error event")
	}

	got, err = runner.Run(context.Background(), session, "resume")
	if err != nil {
		t.Fatalf("resumed Run() error = %v", err)
	}
	if got != "resumed" {
		t.Fatalf("resumed Run() result = %q, want %q", got, "resumed")
	}
	if len(client.recordedRequests()) != 3 {
		t.Fatalf("request count after resume = %d, want 3", len(client.recordedRequests()))
	}
	assertCompleteToolHistory(t, client.recordedRequests()[2].Messages)
}

func TestExecToolCallRejectsInvalidModelOutput(t *testing.T) {
	runner := &Runner{}
	session := newTestSession(t, Options{})

	tests := []struct {
		name string
		call provider.ToolCall
		want string
	}{
		{
			name: "unknown tool",
			call: toolCall("call-unknown", "not_a_real_tool", `{}`),
			want: "unknown tool: not_a_real_tool",
		},
		{
			name: "malformed arguments",
			call: toolCall("call-malformed", "shell", `not-json`),
			want: "invalid tool arguments:",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got, isError := runner.execToolCall(context.Background(), session, test.call)
			if !strings.Contains(got, test.want) {
				t.Fatalf("execToolCall() = %q, want it to contain %q", got, test.want)
			}
			if !isError {
				t.Fatalf("execToolCall() isError = false, want true for %q", got)
			}
		})
	}
}

func TestRunnerStoresOpaqueAndMarksToolErrors(t *testing.T) {
	denied := provider.ToolCall{ID: "c1", Name: "write_file", Arguments: `{"path":"../x","content":"y"}`}
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{denied}, Opaque: json.RawMessage(`{"k":1}`)}},
		{result: &provider.TurnResult{Text: "ok"}},
	}}
	session := newTestSession(t, Options{Provider: client, Sandbox: "workspace-write", Approval: "never"})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	if _, err := runner.Run(context.Background(), session, "go"); err != nil {
		t.Fatal(err)
	}
	history := client.recordedRequests()[1].Messages
	assistant, tool := history[1], history[2]
	if string(assistant.Opaque) != `{"k":1}` {
		t.Fatalf("assistant opaque = %s", assistant.Opaque)
	}
	if tool.Role != provider.RoleTool || !tool.IsError || !strings.Contains(tool.Text, "denied") {
		t.Fatalf("tool message = %#v", tool)
	}
}

func TestRunnerShellNonZeroExitIsNotToolError(t *testing.T) {
	call := provider.ToolCall{ID: "c1", Name: "shell", Arguments: `{"command":"exit 3"}`}
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
		{result: &provider.TurnResult{Text: "ok"}},
	}}
	session := newTestSession(t, Options{Provider: client, Sandbox: "danger-full-access", Approval: "never"})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	if _, err := runner.Run(context.Background(), session, "go"); err != nil {
		t.Fatal(err)
	}
	tool := client.recordedRequests()[1].Messages[2]
	if tool.IsError || !strings.HasPrefix(tool.Text, "exit code: 3") {
		t.Fatalf("tool message = %#v", tool)
	}
}

func TestRunnerRejectsTruncatedToolArguments(t *testing.T) {
	// 500 bytes total; the 200-byte cut lands inside the 63rd euro sign, so
	// truncation must back up to the previous UTF-8 boundary.
	longArguments := `{"command":"` + strings.Repeat("€", 162) + "é"
	tests := []struct {
		name      string
		arguments string
		received  string
	}{
		{
			name:      "short truncated arguments",
			arguments: `{"command":"ls`,
			received:  `received: {"command":"ls`,
		},
		{
			name:      "500-byte arguments truncate on a UTF-8 boundary",
			arguments: longArguments,
			received:  `received: {"command":"` + strings.Repeat("€", 62) + "…",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			call := provider.ToolCall{ID: "c1", Name: "shell", Arguments: test.arguments}
			client := &stubProvider{turns: []stubTurn{
				{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
				{result: &provider.TurnResult{Text: "ok"}},
			}}
			emitter := &recEmitter{}
			session := newTestSession(t, Options{Provider: client, Sandbox: "danger-full-access", Approval: "never"})
			runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}
			if _, err := runner.Run(context.Background(), session, "go"); err != nil {
				t.Fatal(err)
			}
			tool := client.recordedRequests()[1].Messages[2]
			if !tool.IsError || !strings.HasPrefix(tool.Text, "invalid tool arguments:") {
				t.Fatalf("tool message = %#v", tool)
			}
			if !utf8.ValidString(tool.Text) {
				t.Fatalf("tool message is not valid UTF-8: %q", tool.Text)
			}
			if !strings.Contains(tool.Text, test.received) {
				t.Fatalf("tool message = %q, want it to contain %q", tool.Text, test.received)
			}
			if len(test.arguments) > 200 && strings.Contains(tool.Text, test.arguments) {
				t.Fatalf("tool message contains the untruncated arguments")
			}
			if containsEventType(t, emitter.recordedEvents(), "exec_command_begin") {
				t.Fatal("truncated arguments must not execute the tool")
			}
		})
	}
}

func TestRunnerBusy(t *testing.T) {
	unblock := make(chan struct{})
	entered := make(chan struct{}, 1)
	client := &stubProvider{
		turns:   []stubTurn{{result: &provider.TurnResult{Text: "first done"}}},
		block:   unblock,
		entered: entered,
	}
	session := newTestSession(t, Options{Provider: client})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

	firstResult := make(chan struct {
		answer string
		err    error
	}, 1)
	go func() {
		answer, err := runner.Run(context.Background(), session, "first")
		firstResult <- struct {
			answer string
			err    error
		}{answer: answer, err: err}
	}()

	select {
	case <-entered:
	case <-time.After(2 * time.Second):
		t.Fatal("first Run() did not reach the blocked client")
	}

	secondResult := make(chan error, 1)
	go func() {
		_, err := runner.Run(context.Background(), session, "second")
		secondResult <- err
	}()
	select {
	case err := <-secondResult:
		if !errors.Is(err, ErrBusy) {
			t.Fatalf("concurrent Run() error = %v, want ErrBusy", err)
		}
	case <-time.After(time.Second):
		t.Fatal("concurrent Run() blocked instead of returning ErrBusy")
	}

	close(unblock)
	select {
	case result := <-firstResult:
		if result.err != nil || result.answer != "first done" {
			t.Fatalf("first Run() = (%q, %v)", result.answer, result.err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("first Run() did not finish after unblocking the client")
	}
}

func TestManagerBasics(t *testing.T) {
	manager := NewManager()
	if session, ok := manager.Get("missing"); ok || session != nil {
		t.Fatalf("Get(missing) = (%#v, %v), want (nil, false)", session, ok)
	}

	first := manager.Create(Options{})
	second := manager.Create(Options{})
	if first.ID == "" || second.ID == "" {
		t.Fatalf("Create() returned an empty ID: %q, %q", first.ID, second.ID)
	}
	if first.ID == second.ID {
		t.Fatalf("Create() returned duplicate ID %q", first.ID)
	}
	if got, ok := manager.Get(first.ID); !ok || got != first {
		t.Fatalf("Get(first.ID) = (%#v, %v), want first session", got, ok)
	}
	if first.model != "deepseek-v4-pro" || first.maxTurns != DefaultMaxTurns || first.system != DefaultSystemPrompt {
		t.Fatalf("defaults = model %q, max turns %d, system %q", first.model, first.maxTurns, first.system)
	}
	if len(first.messages) != 0 {
		t.Fatalf("initial messages = %#v, want none", first.messages)
	}
}

func TestManagerReasoningEffort(t *testing.T) {
	manager := NewManager()

	if got := manager.Create(Options{}).reasoningEffort; got != "high" {
		t.Fatalf("default reasoning effort = %q, want %q", got, "high")
	}
	if got := manager.Create(Options{ReasoningEffort: "low"}).reasoningEffort; got != "low" {
		t.Fatalf("explicit reasoning effort = %q, want %q", got, "low")
	}
	if got := manager.Create(Options{ReasoningEffort: "low"}).effortSent; got != "low" {
		t.Fatalf("effort sent default = %q, want it to default to the requested %q", got, "low")
	}
	if got := manager.Create(Options{ReasoningEffort: "xhigh", EffortSent: "max"}).effortSent; got != "max" {
		t.Fatalf("explicit effort sent = %q, want %q", got, "max")
	}
}

func TestManagerEvictsIdleSessions(t *testing.T) {
	manager := NewManager()
	idle := manager.Create(Options{})
	idle.lastUsed = time.Now().Add(-25 * time.Hour)
	fresh := manager.Create(Options{})

	if got, ok := manager.Get(idle.ID); ok || got != nil {
		t.Fatalf("Get(idle.ID) = (%#v, %v), want (nil, false)", got, ok)
	}
	if got, ok := manager.Get(fresh.ID); !ok || got != fresh {
		t.Fatalf("Get(fresh.ID) = (%#v, %v), want fresh session", got, ok)
	}
}

func TestManagerKeepsBusySessions(t *testing.T) {
	manager := NewManager()
	busy := manager.Create(Options{})
	busy.lastUsed = time.Now().Add(-25 * time.Hour)
	busy.mu.Lock()
	defer busy.mu.Unlock()

	fresh := manager.Create(Options{})

	if got, ok := manager.Get(busy.ID); !ok || got != busy {
		t.Fatalf("Get(busy.ID) = (%#v, %v), want busy session", got, ok)
	}
	if got, ok := manager.Get(fresh.ID); !ok || got != fresh {
		t.Fatalf("Get(fresh.ID) = (%#v, %v), want fresh session", got, ok)
	}
}

func TestManagerCapsSessionCount(t *testing.T) {
	manager := NewManager()
	current := time.Date(2024, 1, 1, 0, 0, 0, 0, time.UTC)
	manager.now = func() time.Time { return current }

	sessions := make([]*Session, 0, maxSessions)
	for i := 0; i < maxSessions; i++ {
		sessions = append(sessions, manager.Create(Options{}))
		current = current.Add(time.Minute)
	}
	newest := manager.Create(Options{})

	manager.mu.Lock()
	count := len(manager.sessions)
	manager.mu.Unlock()
	if count != maxSessions {
		t.Fatalf("session count = %d, want %d", count, maxSessions)
	}
	if got, ok := manager.Get(sessions[0].ID); ok || got != nil {
		t.Fatalf("Get(oldest.ID) = (%#v, %v), want (nil, false)", got, ok)
	}
	if got, ok := manager.Get(sessions[1].ID); !ok || got != sessions[1] {
		t.Fatalf("Get(secondOldest.ID) = (%#v, %v), want it present", got, ok)
	}
	if got, ok := manager.Get(newest.ID); !ok || got != newest {
		t.Fatalf("Get(newest.ID) = (%#v, %v), want it present", got, ok)
	}
}

func TestRunnerUpdatesLastUsed(t *testing.T) {
	client := &stubProvider{turns: []stubTurn{{result: &provider.TurnResult{Text: "done"}}}}
	session := newTestSession(t, Options{Provider: client})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

	before := time.Now()
	if _, err := runner.Run(context.Background(), session, "finish the task"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	if !session.lastUsed.After(before) {
		t.Fatalf("session.lastUsed = %v, want after %v", session.lastUsed, before)
	}
}

func TestRunnerClientErrorPreservesSessionAndUnlocks(t *testing.T) {
	client := &stubProvider{turns: []stubTurn{
		{err: errors.New("upstream failed")},
		{result: &provider.TurnResult{Text: "recovered"}},
	}}
	emitter := &recEmitter{}
	session := newTestSession(t, Options{Provider: client})
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}

	if _, err := runner.Run(context.Background(), session, "first prompt"); err == nil || err.Error() != "upstream failed" {
		t.Fatalf("first Run() error = %v", err)
	}
	if len(session.messages) != 1 || session.messages[0].Role != provider.RoleUser || session.messages[0].Text != "first prompt" {
		t.Fatalf("messages after client error = %#v", session.messages)
	}
	if gotTypes := eventTypes(t, emitter.recordedEvents()); !reflect.DeepEqual(gotTypes, []string{"task_started", "error"}) {
		t.Fatalf("first run event types = %v", gotTypes)
	}

	got, err := runner.Run(context.Background(), session, "second prompt")
	if err != nil || got != "recovered" {
		t.Fatalf("second Run() = (%q, %v)", got, err)
	}
	if len(session.messages) != 3 {
		t.Fatalf("message count after recovery = %d, want 3", len(session.messages))
	}
}

func TestRunnerEmitsDeltasAndUsageInOrder(t *testing.T) {
	client := &stubProvider{turns: []stubTurn{{
		result: &provider.TurnResult{
			Text: "done",
			Usage: &provider.Usage{
				Input:  7,
				Output: 5,
				Total:  12,
			},
		},
		deltas: []string{"do", "ne"},
	}}}
	emitter := &recEmitter{}
	session := newTestSession(t, Options{Provider: client})
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}

	if _, err := runner.Run(context.Background(), session, "stream"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	events := emitter.recordedEvents()
	if gotTypes := eventTypes(t, events); !reflect.DeepEqual(gotTypes, []string{
		"task_started",
		"agent_message_delta",
		"agent_message_delta",
		"token_count",
		"agent_message",
		"task_complete",
	}) {
		t.Fatalf("event types = %v", gotTypes)
	}
	if events[1]["delta"] != "do" || events[2]["delta"] != "ne" {
		t.Fatalf("delta events = %#v, %#v", events[1], events[2])
	}
	if events[3]["prompt_tokens"] != 7 || events[3]["completion_tokens"] != 5 || events[3]["total_tokens"] != 12 {
		t.Fatalf("token event = %#v", events[3])
	}
}

func TestBuiltinTools(t *testing.T) {
	tools := BuiltinTools()
	if len(tools) != 4 {
		t.Fatalf("builtin tool count = %d, want 4", len(tools))
	}

	wantRequired := map[string][]string{
		"shell":       {"command"},
		"read_file":   {"path"},
		"write_file":  {"path", "content"},
		"apply_patch": {"patch"},
	}
	for _, tool := range tools {
		if tool.Name == "" || tool.Description == "" || tool.Parameters == nil {
			t.Fatalf("invalid tool declaration: %#v", tool)
		}
		if !strings.Contains(strings.ToLower(tool.Description), "sandbox policy") ||
			!strings.Contains(strings.ToLower(tool.Description), "justification") {
			t.Errorf("%s description does not explain sandbox/justification: %q", tool.Name, tool.Description)
		}
		var schema struct {
			Type       string `json:"type"`
			Properties map[string]struct {
				Type        string `json:"type"`
				Description string `json:"description"`
			} `json:"properties"`
			Required []string `json:"required"`
		}
		if err := json.Unmarshal(tool.Parameters, &schema); err != nil {
			t.Fatalf("decode %s schema: %v", tool.Name, err)
		}
		if schema.Type != "object" {
			t.Errorf("%s schema type = %q", tool.Name, schema.Type)
		}
		if !reflect.DeepEqual(schema.Required, wantRequired[tool.Name]) {
			t.Errorf("%s required = %v, want %v", tool.Name, schema.Required, wantRequired[tool.Name])
		}
		if _, ok := schema.Properties["justification"]; !ok {
			t.Errorf("%s schema lacks justification", tool.Name)
		}
		if tool.Name == "shell" {
			timeout := schema.Properties["timeout_seconds"]
			if timeout.Type != "integer" || !strings.Contains(timeout.Description, "clamped, max 600") {
				t.Errorf("shell timeout_seconds = %#v", timeout)
			}
		}
		if tool.Name == "read_file" {
			offset := schema.Properties["offset"]
			if offset.Type != "integer" || !strings.Contains(offset.Description, "1-based") {
				t.Errorf("read_file offset = %#v", offset)
			}
			limit := schema.Properties["limit"]
			if limit.Type != "integer" || !strings.Contains(limit.Description, "lines") {
				t.Errorf("read_file limit = %#v", limit)
			}
		}
	}
}

func runPatchOnce(t *testing.T, options Options, approver *stubApprover, patchText string) (string, *recEmitter) {
	t.Helper()
	arguments, err := json.Marshal(map[string]string{"patch": patchText})
	if err != nil {
		t.Fatal(err)
	}
	call := toolCall("call-patch", "apply_patch", string(arguments))
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
		{result: &provider.TurnResult{Text: "done"}},
	}}
	emitter := &recEmitter{}
	options.Provider = client
	session := newTestSession(t, options)
	runner := &Runner{Emitter: emitter, Approver: approver}
	if _, err := runner.Run(context.Background(), session, "patch it"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	return findToolMessage(t, client.recordedRequests()[1].Messages, call.ID).Text, emitter
}

func TestRunnerApplyPatchUpdatesFile(t *testing.T) {
	cwd := t.TempDir()
	if err := os.WriteFile(filepath.Join(cwd, "a.txt"), []byte("old\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	result, emitter := runPatchOnce(t, Options{Cwd: cwd}, &stubApprover{},
		"*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** End Patch")

	if !strings.HasPrefix(result, "Success. Updated the following files:") {
		t.Fatalf("result = %q", result)
	}
	data, _ := os.ReadFile(filepath.Join(cwd, "a.txt"))
	if string(data) != "new\n" {
		t.Fatalf("a.txt = %q", data)
	}
	events := emitter.recordedEvents()
	begin := events[1]
	if begin["type"] != "exec_command_begin" || begin["tool"] != "apply_patch" ||
		!reflect.DeepEqual(begin["paths"], []string{"a.txt"}) {
		t.Fatalf("begin event = %#v", begin)
	}
	if end := events[2]; end["type"] != "exec_command_end" || !reflect.DeepEqual(end["paths"], []string{"a.txt"}) {
		t.Fatalf("end event = %#v", end)
	}
}

func TestRunnerApplyPatchDeniesAnyPathOutsideCwd(t *testing.T) {
	cwd := t.TempDir()
	result, emitter := runPatchOnce(t, Options{Cwd: cwd, Sandbox: "workspace-write", Approval: "never"}, &stubApprover{},
		"*** Begin Patch\n*** Add File: inside.txt\n+x\n*** Add File: ../outside.txt\n+y\n*** End Patch")

	if !strings.Contains(result, "denied") {
		t.Fatalf("result = %q, want denial", result)
	}
	if _, err := os.Stat(filepath.Join(cwd, "inside.txt")); !os.IsNotExist(err) {
		t.Fatalf("inside.txt written despite denial")
	}
	if _, err := os.Stat(filepath.Join(filepath.Dir(cwd), "outside.txt")); !os.IsNotExist(err) {
		t.Fatalf("outside.txt written despite denial")
	}
	if containsEventType(t, emitter.recordedEvents(), "exec_command_begin") {
		t.Fatalf("denied patch emitted exec_command_begin")
	}
}

func TestRunnerApplyPatchSingleApprovalListsPaths(t *testing.T) {
	cwd := t.TempDir()
	approver := &stubApprover{approved: true}
	result, _ := runPatchOnce(t, Options{Cwd: cwd, Sandbox: "read-only", Approval: "on-request"}, approver,
		"*** Begin Patch\n*** Add File: a.txt\n+x\n*** Add File: b.txt\n+y\n*** End Patch")

	if !strings.HasPrefix(result, "Success.") {
		t.Fatalf("result = %q", result)
	}
	requests := approver.recordedRequests()
	if len(requests) != 1 || requests[0].Tool != "apply_patch" || requests[0].Path != "a.txt, b.txt" {
		t.Fatalf("approval requests = %#v", requests)
	}
}

func TestRunnerApplyPatchInvalidPatch(t *testing.T) {
	result, _ := runPatchOnce(t, Options{}, &stubApprover{}, "not a patch")
	if !strings.HasPrefix(result, "error: invalid patch:") {
		t.Fatalf("result = %q", result)
	}
}

func newTestSession(t *testing.T, options Options) *Session {
	t.Helper()
	if options.Cwd == "" {
		options.Cwd = t.TempDir()
	}
	if options.Sandbox == "" {
		options.Sandbox = policy.Sandbox("workspace-write")
	}
	if options.Approval == "" {
		options.Approval = policy.ApprovalPolicy("never")
	}
	return NewManager().Create(options)
}

func toolCall(id, name, arguments string) provider.ToolCall {
	return provider.ToolCall{
		ID:        id,
		Name:      name,
		Arguments: arguments,
	}
}

func findToolMessage(t *testing.T, messages []provider.Message, callID string) provider.Message {
	t.Helper()
	for _, message := range messages {
		if message.Role == provider.RoleTool && message.ToolCallID == callID {
			return message
		}
	}
	t.Fatalf("no tool message found for call ID %q in %#v", callID, messages)
	return provider.Message{}
}

func eventTypes(t *testing.T, events []map[string]any) []string {
	t.Helper()
	types := make([]string, len(events))
	for i, event := range events {
		eventType, ok := event["type"].(string)
		if !ok {
			t.Fatalf("event %d has invalid type: %#v", i, event)
		}
		types[i] = eventType
	}
	return types
}

func containsEventType(t *testing.T, events []map[string]any, want string) bool {
	t.Helper()
	for _, eventType := range eventTypes(t, events) {
		if eventType == want {
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

func assertCompleteToolHistory(t *testing.T, messages []provider.Message) {
	t.Helper()

	toolResponses := make(map[string][]provider.Message)
	for _, message := range messages {
		if message.Role != provider.RoleTool {
			continue
		}
		if message.Text == "" {
			t.Fatalf("tool response %q has empty Text", message.ToolCallID)
		}
		toolResponses[message.ToolCallID] = append(toolResponses[message.ToolCallID], message)
	}

	for _, message := range messages {
		if message.Role != provider.RoleAssistant {
			continue
		}
		for _, call := range message.ToolCalls {
			responses := toolResponses[call.ID]
			if len(responses) != 1 {
				t.Fatalf("tool call %q has %d responses, want exactly 1", call.ID, len(responses))
			}
		}
	}
}

func runShellOnce(t *testing.T, options Options, approver *stubApprover, command string) string {
	t.Helper()
	call := toolCall("call-shell", "shell", `{"command":`+strconv.Quote(command)+`}`)
	client := &stubProvider{turns: []stubTurn{
		{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{call}}},
		{result: &provider.TurnResult{Text: "done"}},
	}}
	options.Provider = client
	session := newTestSession(t, options)
	runner := &Runner{Emitter: &recEmitter{}, Approver: approver}
	if _, err := runner.Run(context.Background(), session, "run it"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}
	return findToolMessage(t, client.recordedRequests()[1].Messages, call.ID).Text
}

func TestRunnerShellSandboxing(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		t.Skipf("landlock unavailable: %v", err)
	}
	outside := t.TempDir()
	extra := t.TempDir()

	t.Run("workspace-write denies writes outside roots", func(t *testing.T) {
		// t.TempDir() lives under /tmp, which is itself a workspace-write root, so
		// the target has to sit outside every root.
		target := filepath.Join("/var/tmp", "subagent-mcp-sandbox-outside-"+strconv.Itoa(os.Getpid()))
		t.Cleanup(func() { _ = os.Remove(target) })
		result := runShellOnce(t, Options{Sandbox: "workspace-write", Approval: "never"}, &stubApprover{}, "touch "+target)
		if strings.HasPrefix(result, "exit code: 0") {
			t.Fatalf("result = %q, want failure", result)
		}
		if _, err := os.Stat(target); err == nil {
			t.Fatalf("outside file created")
		}
	})

	t.Run("workspace-write allows cwd and /tmp", func(t *testing.T) {
		tmpFile := filepath.Join("/tmp", "subagent-mcp-sandbox-"+strconv.Itoa(os.Getpid()))
		t.Cleanup(func() { _ = os.Remove(tmpFile) })
		result := runShellOnce(t, Options{Sandbox: "workspace-write", Approval: "never"}, &stubApprover{}, "touch in-cwd && touch "+tmpFile)
		if !strings.HasPrefix(result, "exit code: 0") {
			t.Fatalf("result = %q, want success", result)
		}
	})

	t.Run("workspace-write allows configured writable root", func(t *testing.T) {
		target := filepath.Join(extra, "extra")
		result := runShellOnce(t, Options{Sandbox: "workspace-write", Approval: "never", WritableRoots: []string{extra}}, &stubApprover{}, "touch "+target)
		if !strings.HasPrefix(result, "exit code: 0") {
			t.Fatalf("result = %q, want success", result)
		}
	})

	t.Run("danger-full-access is unwrapped", func(t *testing.T) {
		target := filepath.Join(outside, "danger")
		result := runShellOnce(t, Options{Sandbox: "danger-full-access", Approval: "never"}, &stubApprover{}, "touch "+target)
		if !strings.HasPrefix(result, "exit code: 0") {
			t.Fatalf("result = %q, want success", result)
		}
	})

	t.Run("human-approved command is unwrapped", func(t *testing.T) {
		target := filepath.Join(outside, "approved")
		result := runShellOnce(t, Options{Sandbox: "workspace-write", Approval: "untrusted"}, &stubApprover{approved: true}, "touch "+target)
		if !strings.HasPrefix(result, "exit code: 0") {
			t.Fatalf("result = %q, want success", result)
		}
	})

	t.Run("read-only allowlisted command still runs", func(t *testing.T) {
		result := runShellOnce(t, Options{Sandbox: "read-only", Approval: "never"}, &stubApprover{}, "echo hi")
		if !strings.HasPrefix(result, "exit code: 0") || !strings.Contains(result, "hi") {
			t.Fatalf("result = %q, want success", result)
		}
	})

	t.Run("workspace-write denies /dev/shm but allows /dev/null", func(t *testing.T) {
		if info, err := os.Stat("/dev/shm"); err != nil || !info.IsDir() {
			t.Skip("/dev/shm unavailable")
		}
		probe := filepath.Join("/dev/shm", "subagent-mcp-runner-probe-"+strconv.Itoa(os.Getpid()))
		t.Cleanup(func() { _ = os.Remove(probe) })
		result := runShellOnce(t, Options{Sandbox: "workspace-write", Approval: "never"}, &stubApprover{},
			"echo x > /dev/null && touch "+probe)
		if strings.HasPrefix(result, "exit code: 0") || !strings.Contains(result, "Permission denied") {
			t.Fatalf("result = %q, want /dev/shm permission denied", result)
		}
	})
}
