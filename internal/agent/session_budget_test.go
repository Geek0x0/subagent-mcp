package agent

import (
	"context"
	"encoding/json"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/rollout"
)

// TestMessageSizeCountsEverythingTheNextTurnResends pins the parts of a history
// entry that count against the history budget: the replay payload and every tool
// call's arguments cost the next request just as much as the text does.
func TestMessageSizeCountsEverythingTheNextTurnResends(t *testing.T) {
	tests := []struct {
		name string
		msg  provider.Message
		want int64
	}{
		{name: "empty", msg: provider.Message{}, want: 0},
		{name: "text", msg: provider.Message{Text: "hello"}, want: 5},
		{
			name: "tool call arguments",
			msg: provider.Message{ToolCalls: []provider.ToolCall{
				{ID: "ab", Name: "cde", Arguments: "fghi"},
			}},
			want: 9,
		},
		{
			name: "opaque replay payload",
			msg:  provider.Message{Opaque: []byte(`{"a":1}`)},
			want: 7,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := messageSize(test.msg); got != test.want {
				t.Errorf("messageSize(%+v) = %d, want %d", test.msg, got, test.want)
			}
		})
	}
}

// TestHistoryBytesFollowAppendAndTruncate pins that the counter tracks the
// history it measures, including the rollback a run performs when its first
// model call fails: a prompt that was never answered must not count against a
// later reply.
func TestHistoryBytesFollowAppendAndTruncate(t *testing.T) {
	s := &Session{}

	s.appendMessage(provider.Message{Role: provider.RoleUser, Text: "first"})
	if got := s.historyBytes; got != 5 {
		t.Fatalf("historyBytes after one append = %d, want 5", got)
	}

	entry := len(s.messages)
	s.appendMessage(provider.Message{Role: provider.RoleUser, Text: "second"})
	if got := s.historyBytes; got != 11 {
		t.Fatalf("historyBytes after two appends = %d, want 11", got)
	}

	s.truncateMessages(entry)
	if got := s.historyBytes; got != 5 {
		t.Errorf("historyBytes after rollback = %d, want 5", got)
	}
	if got := len(s.messages); got != 1 {
		t.Errorf("messages after rollback = %d, want 1", got)
	}

	s.truncateMessages(0)
	if got := s.historyBytes; got != 0 {
		t.Errorf("historyBytes after rolling back everything = %d, want 0", got)
	}
}

// TestBudgetErrorReportsTheSpentLimit pins which budget refuses a turn and that
// each limit is named, since the caller has to know which one to raise.
func TestBudgetErrorReportsTheSpentLimit(t *testing.T) {
	tests := []struct {
		name    string
		session *Session
		want    string
	}{
		{name: "no limits configured", session: &Session{}, want: ""},
		{
			name:    "history over its limit",
			session: &Session{maxHistoryBytes: 10, historyBytes: 11},
			want:    "max_history_bytes",
		},
		{
			name:    "history exactly at its limit is allowed",
			session: &Session{maxHistoryBytes: 10, historyBytes: 10},
			want:    "",
		},
		{
			name:    "tokens at their limit",
			session: &Session{maxTotalTokens: 100, totalUsage: provider.Usage{Total: 100}},
			want:    "max_total_tokens",
		},
		{
			name:    "tokens below their limit",
			session: &Session{maxTotalTokens: 100, totalUsage: provider.Usage{Total: 99}},
			want:    "",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			err := test.session.budgetError()
			if test.want == "" {
				if err != nil {
					t.Fatalf("budgetError() = %v, want nil", err)
				}
				return
			}
			if err == nil {
				t.Fatalf("budgetError() = nil, want an error naming %q", test.want)
			}
			if !strings.Contains(err.Error(), test.want) {
				t.Errorf("budgetError() = %v, want it to name %q", err, test.want)
			}
		})
	}
}

// TestRecordThreadEventCountsReportedUsage pins that a delegated thread's token
// events reach the session total, so the token budget covers providers that run
// whole threads and never return a TurnResult.
//
// The values are Go ints because that is what an adapter building the event in
// process produces; the float64 case covers the same event decoded from JSON.
// Testing only one of the two shapes is how the accounting can silently record
// zero for every real event.
func TestRecordThreadEventCountsReportedUsage(t *testing.T) {
	tests := []struct {
		name  string
		value func(int) any
	}{
		{name: "int, as an adapter builds it", value: func(v int) any { return v }},
		{name: "int64", value: func(v int) any { return int64(v) }},
		{name: "float64, as JSON decodes it", value: func(v int) any { return float64(v) }},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			s := &Session{}
			r := &Runner{}

			r.recordThreadEvent(s, map[string]any{
				"type": "token_count", "prompt_tokens": test.value(60),
				"completion_tokens": test.value(40), "total_tokens": test.value(100),
			})
			if got := s.totalUsage; got != (provider.Usage{Input: 60, Output: 40, Total: 100}) {
				t.Fatalf("usage after one token_count = %+v, want input 60, output 40, total 100", got)
			}

			// A second turn accumulates, and a non-usage event changes nothing.
			r.recordThreadEvent(s, map[string]any{"type": "task_completed"})
			r.recordThreadEvent(s, map[string]any{
				"type": "token_count", "prompt_tokens": test.value(1),
				"completion_tokens": test.value(2), "total_tokens": test.value(3),
			})
			if got := s.totalUsage.Total; got != 103 {
				t.Errorf("cumulative tokens = %d, want 103", got)
			}
		})
	}
}

// TestThreadRunHonorsTheTokenBudget pins that a delegated thread spends the same
// budget as the built-in loop: the ceiling is checked before the thread is handed
// the prompt, so a session that already spent it cannot start another turn.
func TestThreadRunHonorsTheTokenBudget(t *testing.T) {
	runs := 0
	thread := &stubThread{run: func(_ context.Context, _ string, _ string, cb provider.ThreadCallbacks) (string, error) {
		runs++
		// The codex adapter builds this event in process, so the values are
		// Go ints; recording zero here would make the budget inert.
		cb.Emit(map[string]any{
			"type": "token_count", "prompt_tokens": 4000,
			"completion_tokens": 1000, "total_tokens": 5000,
		})
		return "done", nil
	}}
	runner := &Runner{Emitter: emitterFunc(func(context.Context, string, map[string]any) {})}
	session := newTestSession(t, Options{Thread: thread, MaxTotalTokens: 100})

	if _, err := runner.Run(context.Background(), session, "first"); err != nil {
		t.Fatalf("first Run() error = %v, want success", err)
	}
	if runs != 1 {
		t.Fatalf("thread runs after the first prompt = %d, want 1", runs)
	}
	if got := session.totalUsage.Total; got != 5000 {
		t.Fatalf("recorded tokens = %d, want 5000", got)
	}

	_, err := runner.Run(context.Background(), session, "second")
	if err == nil || !strings.Contains(err.Error(), "max_total_tokens") {
		t.Fatalf("second Run() error = %v, want it to refuse the spent budget by name", err)
	}
	if runs != 1 {
		t.Errorf("thread runs after the budget was spent = %d, want 1", runs)
	}
}

// TestThreadRunHonorsTheHistoryBudget pins the other ceiling on the same path: a
// thread session whose history is over the limit (set directly, since a delegated
// thread does not build one) is refused before its next turn.
func TestThreadRunHonorsTheHistoryBudget(t *testing.T) {
	thread := &stubThread{run: func(context.Context, string, string, provider.ThreadCallbacks) (string, error) {
		return "done", nil
	}}
	runner := &Runner{Emitter: emitterFunc(func(context.Context, string, map[string]any) {})}
	session := newTestSession(t, Options{Thread: thread, MaxHistoryBytes: 10})
	session.historyBytes = 11

	_, err := runner.Run(context.Background(), session, "prompt")
	if err == nil || !strings.Contains(err.Error(), "max_history_bytes") {
		t.Fatalf("Run() error = %v, want it to refuse the spent history budget by name", err)
	}
}

// TestThreadRunNamesTheRunDeadline pins that a deadline which fires during a
// delegated turn reads as the deadline, not as a bare context error.
func TestThreadRunNamesTheRunDeadline(t *testing.T) {
	thread := &stubThread{run: func(ctx context.Context, _ string, _ string, _ provider.ThreadCallbacks) (string, error) {
		<-ctx.Done()
		return "", ctx.Err()
	}}
	runner := &Runner{Emitter: emitterFunc(func(context.Context, string, map[string]any) {})}
	session := newTestSession(t, Options{Thread: thread, RunDeadline: 100 * time.Millisecond})

	_, err := runner.Run(context.Background(), session, "prompt")
	if err == nil || !strings.Contains(err.Error(), "run deadline") {
		t.Fatalf("Run() error = %v, want it to name the run deadline", err)
	}
}

// TestDeltaCoalescerMergesWithoutLosingText pins the stream contract: every byte
// the provider streamed is emitted, in order, in one or more events. Sending one
// event per token is what overflowed the client's bounded notification channel
// and lost most of a long reply.
func TestDeltaCoalescerMergesWithoutLosingText(t *testing.T) {
	var emitted []string
	c := newDeltaCoalescer(func(text string) { emitted = append(emitted, text) })
	// Hold the time-based flush open so the buffering below is deterministic
	// rather than dependent on how fast the test machine runs.
	c.interval = time.Hour

	// Small deltas arrive faster than the flush interval, so they merge.
	for _, delta := range []string{"Hel", "lo ", "wor", "ld"} {
		c.add(delta)
	}
	if len(emitted) != 0 {
		t.Fatalf("emitted %v before any flush, want the deltas still buffered", emitted)
	}

	c.flush()
	if len(emitted) != 1 || emitted[0] != "Hello world" {
		t.Fatalf("emitted = %v, want one event carrying the whole stream", emitted)
	}

	// A flush with nothing buffered emits nothing, so a turn that streamed no
	// text does not produce an empty event.
	c.flush()
	if len(emitted) != 1 {
		t.Fatalf("emitted = %v after an empty flush, want no new event", emitted)
	}
}

// TestDeltaCoalescerFlushesOnSize pins the size bound: a stream that never waits
// long enough for the interval still emits as it goes, so a huge reply cannot
// accumulate in memory or arrive as one late event.
func TestDeltaCoalescerFlushesOnSize(t *testing.T) {
	var emitted []string
	c := newDeltaCoalescer(func(text string) { emitted = append(emitted, text) })
	c.interval = time.Hour

	chunk := strings.Repeat("x", deltaFlushBytes/2)
	for i := 0; i < 4; i++ {
		c.add(chunk)
	}
	if len(emitted) < 2 {
		t.Fatalf("emitted %d event(s) for %d bytes, want a flush before the buffer reached %d", len(emitted), 4*len(chunk), deltaFlushBytes)
	}
	c.flush()

	total := 0
	for _, text := range emitted {
		total += len(text)
	}
	if total != 4*len(chunk) {
		t.Errorf("emitted %d bytes, want %d", total, 4*len(chunk))
	}
}

// TestThreadUsageReachesTheRollout pins that a delegated thread's reported usage
// is recorded where the built-in loop records it. A thread provider returns no
// TurnResult, so without this the rollout of every codex session records no
// tokens at all, even though the client saw them streamed.
func TestThreadUsageReachesTheRollout(t *testing.T) {
	t.Setenv("CODEX_HOME", t.TempDir())

	recorder := rollout.Open("thread-usage", time.Now())
	if recorder == nil {
		t.Fatal("rollout.Open() returned nil; the recorder must be available for this test")
	}

	thread := &stubThread{run: func(_ context.Context, _ string, _ string, cb provider.ThreadCallbacks) (string, error) {
		cb.Emit(map[string]any{
			"type": "token_count", "prompt_tokens": 400,
			"completion_tokens": 100, "total_tokens": 500,
		})
		return "done", nil
	}}
	runner := &Runner{Emitter: emitterFunc(func(context.Context, string, map[string]any) {})}
	session := newTestSession(t, Options{Thread: thread})
	session.AttachRollout(recorder)

	if _, err := runner.Run(context.Background(), session, "prompt"); err != nil {
		t.Fatalf("Run() error = %v", err)
	}

	raw, err := os.ReadFile(recorder.Path())
	if err != nil {
		t.Fatalf("read rollout: %v", err)
	}
	var found map[string]any
	for _, line := range strings.Split(strings.TrimSpace(string(raw)), "\n") {
		var entry struct {
			Type    string         `json:"type"`
			Payload map[string]any `json:"payload"`
		}
		if err := json.Unmarshal([]byte(line), &entry); err != nil {
			t.Fatalf("rollout line %q: %v", line, err)
		}
		if entry.Type == "event_msg" && entry.Payload["type"] == "token_count" {
			found = entry.Payload
		}
	}
	if found == nil {
		t.Fatal("the rollout has no token_count event for a delegated thread that reported usage")
	}
	info, ok := found["info"].(map[string]any)
	if !ok {
		t.Fatalf("token_count payload = %#v, want an info object", found)
	}
	total, ok := info["total_token_usage"].(map[string]any)
	if !ok {
		t.Fatalf("info = %#v, want total_token_usage", info)
	}
	if got := total["total_tokens"]; got != float64(500) {
		t.Errorf("rollout total_tokens = %v, want 500", got)
	}
}
