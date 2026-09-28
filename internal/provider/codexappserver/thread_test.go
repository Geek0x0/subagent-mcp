package codexappserver

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"reflect"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"testing/synctest"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func TestStartThreadParams(t *testing.T) {
	for _, instructions := range []bool{false, true} {
		t.Run(fmt.Sprint(instructions), func(t *testing.T) {
			f := threadFake(t)
			opts := provider.ThreadOptions{Cwd: "/work", Model: "model", Sandbox: "read-only", ApprovalPolicy: "on-failure", Ephemeral: true}
			want := map[string]any{"cwd": "/work", "model": "model", "sandbox": "read-only", "approvalPolicy": "on-request", "ephemeral": true}
			if instructions {
				opts.BaseInstructions, opts.DeveloperInstructions = "base", "developer"
				want["baseInstructions"], want["developerInstructions"] = "base", "developer"
			}
			th := mustStartThread(t, fakePool(t, f), opts)
			assertParams(t, f, "thread/start", want)
			th.Close()
		})
	}
}

func TestRunFinalAnswerByPhase(t *testing.T) {
	for _, tc := range []struct {
		name     string
		messages [][2]string
		want     string
	}{
		{"final answer", [][2]string{{"commentary", "working"}, {"final_answer", "42"}}, "42"},
		{"last final wins", [][2]string{{"final_answer", "41"}, {"final_answer", "42"}, {"commentary", "done"}}, "42"},
		{"fallback", [][2]string{{"commentary", "first"}, {"", "last"}}, "last"},
		{"empty final wins", [][2]string{{"final_answer", ""}, {"commentary", "done"}}, ""},
		{"no messages", nil, ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := threadFake(t)
			scriptTurn(f, "thr-1", "turn-1", func() {
				for _, msg := range tc.messages {
					agentMessage(f, "thr-1", "turn-1", msg[0], msg[1])
				}
				completeTurn(f, "thr-1", "turn-1", "completed")
			})
			th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
			var events []string
			text, err := th.Run(threadContext(t), "question", "high", provider.ThreadCallbacks{Emit: func(event map[string]any) { events = append(events, event["type"].(string)) }})
			if err != nil || text != tc.want {
				t.Fatalf("Run = %q, %v; want %q", text, err, tc.want)
			}
			want := []string{"task_started"}
			for range tc.messages {
				want = append(want, "agent_message")
			}
			want = append(want, "task_complete")
			if !reflect.DeepEqual(events, want) {
				t.Fatalf("events = %v, want %v", events, want)
			}
			assertParams(t, f, "turn/start", map[string]any{"threadId": "thr-1", "input": []any{map[string]any{"type": "text", "text": "question"}}, "effort": "high"})
		})
	}
}

func TestRunWritableRoots(t *testing.T) {
	for _, tc := range []struct {
		sandbox string
		roots   []string
	}{{"workspace-write", []string{"/a", "/b"}}, {"workspace-write", nil}, {"read-only", []string{"/a"}}, {"danger-full-access", []string{"/a"}}} {
		t.Run(fmt.Sprint(tc), func(t *testing.T) {
			f := threadFake(t)
			scriptTurn(f, "thr-1", "turn-1", func() { completeTurn(f, "thr-1", "turn-1", "completed") })
			th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{Sandbox: tc.sandbox, WritableRoots: tc.roots})
			if _, err := th.Run(threadContext(t), "hi", "low", provider.ThreadCallbacks{}); err != nil {
				t.Fatal(err)
			}
			want := map[string]any{"threadId": "thr-1", "input": []any{map[string]any{"type": "text", "text": "hi"}}, "effort": "low"}
			if tc.sandbox == "workspace-write" && len(tc.roots) != 0 {
				want["sandboxPolicy"] = map[string]any{"type": "workspaceWrite", "writableRoots": tc.roots, "networkAccess": false}
			}
			assertParams(t, f, "turn/start", want)
		})
	}
}

func TestRunCommandApproval(t *testing.T) {
	for _, accept := range []bool{true, false} {
		t.Run(fmt.Sprint(accept), func(t *testing.T) {
			f := threadFake(t)
			reply := make(chan json.RawMessage, 1)
			scriptTurn(f, "thr-1", "turn-1", func() {
				reply <- f.Request("item/commandExecution/requestApproval", map[string]string{"threadId": "thr-1", "turnId": "turn-1", "command": "rm x", "reason": "cleanup"})
				completeTurn(f, "thr-1", "turn-1", "completed")
			})
			th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
			var got provider.ApprovalRequest
			_, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{Approve: func(req provider.ApprovalRequest) bool { got = req; return accept }})
			if err != nil {
				t.Fatal(err)
			}
			if want := (provider.ApprovalRequest{Tool: "shell", Command: "rm x", Reason: "cleanup"}); got != want {
				t.Fatalf("approval = %+v, want %+v", got, want)
			}
			decision := "decline"
			if accept {
				decision = "accept"
			}
			assertDecision(t, await(t, reply), decision)
		})
	}
}

func TestRunFileChangeApprovalPaths(t *testing.T) {
	f := threadFake(t)
	reply := make(chan json.RawMessage, 1)
	scriptTurn(f, "thr-1", "turn-1", func() {
		f.Notify("item/started", map[string]any{"threadId": "thr-1", "turnId": "turn-1", "item": map[string]any{"id": "i1", "type": "fileChange", "changes": []any{map[string]string{"path": "/a"}, map[string]string{"path": "/b"}}}})
		reply <- f.Request("item/fileChange/requestApproval", map[string]string{"threadId": "thr-1", "turnId": "turn-1", "itemId": "i1", "reason": "edit"})
		completeTurn(f, "thr-1", "turn-1", "completed")
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	var got provider.ApprovalRequest
	_, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{Approve: func(req provider.ApprovalRequest) bool { got = req; return true }})
	if err != nil {
		t.Fatal(err)
	}
	if want := (provider.ApprovalRequest{Tool: "apply_patch", Path: "/a, /b", Reason: "edit"}); got != want {
		t.Fatalf("approval = %+v, want %+v", got, want)
	}
	assertDecision(t, await(t, reply), "accept")
}

func TestRunUnsupportedAndStaleRequests(t *testing.T) {
	f := threadFake(t)
	replies := make(chan json.RawMessage, 12)
	scriptTurn(f, "thr-1", "turn-1", func() {
		for _, method := range []string{"item/permissions/requestApproval", "item/tool/requestUserInput", "mcpServer/elicitation/request", "item/tool/call", "account/chatgptAuthTokens/refresh", "attestation/generate", "applyPatchApproval", "execCommandApproval"} {
			replies <- f.Request(method, map[string]string{"threadId": "thr-1", "turnId": "turn-1"})
		}
		for _, turn := range []string{"old", ""} {
			replies <- f.Request("item/commandExecution/requestApproval", map[string]string{"threadId": "thr-1", "turnId": turn, "command": "wrong"})
		}
		// Stale notifications must not leak into this turn or finish it.
		agentMessage(f, "thr-1", "old", "final_answer", "wrong")
		completeTurn(f, "thr-1", "old", "completed")
		agentMessage(f, "thr-1", "turn-1", "final_answer", "ok")
		completeTurn(f, "thr-1", "turn-1", "completed")
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	text, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{Approve: func(provider.ApprovalRequest) bool { t.Error("unexpected approval"); return true }})
	if err != nil || text != "ok" {
		t.Fatalf("Run = %q, %v", text, err)
	}
	for range 10 {
		assertRejected(t, await(t, replies))
	}
}

func TestThreadRejectsRequestsBetweenTurns(t *testing.T) {
	f := threadFake(t)
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	request := func() {
		reply := make(chan json.RawMessage, 1)
		go func() {
			reply <- f.Request("item/commandExecution/requestApproval", map[string]string{"threadId": "thr-1", "turnId": "turn-1"})
		}()
		assertRejected(t, await(t, reply))
	}
	request()
	scriptTurn(f, "thr-1", "turn-1", func() { completeTurn(f, "thr-1", "turn-1", "completed") })
	if _, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{}); err != nil {
		t.Fatal(err)
	}
	request()
}

func TestRunFailedTurn(t *testing.T) {
	f := threadFake(t)
	scriptTurn(f, "thr-1", "turn-1", func() {
		f.Notify("turn/completed", map[string]any{"threadId": "thr-1", "turn": map[string]any{"id": "turn-1", "status": "failed", "error": map[string]string{"message": "quota"}}})
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	_, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{})
	if err == nil || !strings.Contains(err.Error(), "quota") {
		t.Fatalf("Run error = %v", err)
	}
}

func TestRunRetryableErrorNotTerminal(t *testing.T) {
	f := threadFake(t)
	scriptTurn(f, "thr-1", "turn-1", func() {
		f.Notify("error", map[string]any{"threadId": "thr-1", "turnId": "turn-1", "willRetry": true, "error": map[string]string{"message": "retry"}})
		f.Notify("error", map[string]any{"threadId": "thr-1", "turnId": "turn-1", "willRetry": false, "error": map[string]string{"message": "informational"}})
		agentMessage(f, "thr-1", "turn-1", "final_answer", "ok")
		completeTurn(f, "thr-1", "turn-1", "completed")
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	var messages []string
	text, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{Emit: func(e map[string]any) {
		if e["type"] == "error" {
			messages = append(messages, e["message"].(string))
		}
	}})
	if err != nil || text != "ok" || !reflect.DeepEqual(messages, []string{"informational"}) {
		t.Fatalf("Run = %q, %v, errors = %v", text, err, messages)
	}
}

func TestRunCancelInterrupts(t *testing.T) {
	f := threadFake(t)
	scriptTurn(f, "thr-1", "turn-1", func() {})
	f.Handle("turn/interrupt", func(json.RawMessage) (any, error) {
		completeTurn(f, "thr-1", "turn-1", "interrupted")
		return map[string]any{}, nil
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	ctx, cancel := context.WithCancel(threadContext(t))
	defer cancel()
	started := make(chan struct{})
	result := make(chan error, 1)
	go func() {
		_, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{Emit: func(e map[string]any) {
			if e["type"] == "task_started" {
				close(started)
			}
		}})
		result <- err
	}()
	await(t, started)
	begin := time.Now()
	cancel()
	if err := await(t, result); !errors.Is(err, context.Canceled) {
		t.Fatalf("Run error = %v", err)
	}
	if elapsed := time.Since(begin); elapsed >= time.Second {
		t.Fatalf("cancel took %s", elapsed)
	}
	assertParams(t, f, "turn/interrupt", map[string]any{"threadId": "thr-1", "turnId": "turn-1"})
	assertSecondRun(t, th, f)
}

func TestRunCancelDuringApproval(t *testing.T) {
	f := threadFake(t)
	reply := make(chan json.RawMessage, 1)
	scriptTurn(f, "thr-1", "turn-1", func() {
		reply <- f.Request("item/commandExecution/requestApproval", map[string]string{"threadId": "thr-1", "turnId": "turn-1", "command": "rm x"})
	})
	f.Handle("turn/interrupt", func(json.RawMessage) (any, error) {
		completeTurn(f, "thr-1", "turn-1", "interrupted")
		return map[string]any{}, nil
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	ctx, cancel := context.WithCancel(threadContext(t))
	defer cancel()
	approving := make(chan struct{})
	result := make(chan error, 1)
	go func() {
		_, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{Approve: func(provider.ApprovalRequest) bool { close(approving); <-ctx.Done(); return false }})
		result <- err
	}()
	await(t, approving)
	// Unsupported and stale requests cannot wait for this approval, either.
	for _, tc := range []struct{ method, turn string }{{"item/permissions/requestApproval", "turn-1"}, {"item/commandExecution/requestApproval", "old"}} {
		unsupported := make(chan json.RawMessage, 1)
		go func() { unsupported <- f.Request(tc.method, map[string]string{"threadId": "thr-1", "turnId": tc.turn}) }()
		assertRejected(t, await(t, unsupported))
	}
	begin := time.Now()
	cancel()
	if err := await(t, result); !errors.Is(err, context.Canceled) {
		t.Fatalf("Run error = %v", err)
	}
	if elapsed := time.Since(begin); elapsed >= 2*time.Second {
		t.Fatalf("cancel took %s", elapsed)
	}
	assertDecision(t, await(t, reply), "decline")
	assertParams(t, f, "turn/interrupt", map[string]any{"threadId": "thr-1", "turnId": "turn-1"})
	assertSecondRun(t, th, f)
}

func TestRunCancelInterruptTimeout(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		f := threadFake(t)
		scriptTurn(f, "thr-1", "turn-1", func() {})
		f.Handle("turn/interrupt", func(json.RawMessage) (any, error) { return map[string]any{}, nil })
		th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
		ctx, cancel := context.WithCancel(t.Context())
		defer cancel()
		begin := time.Now()
		_, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{Emit: func(e map[string]any) {
			if e["type"] == "task_started" {
				cancel()
			}
		}})
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Run error = %v", err)
		}
		if elapsed := time.Since(begin); elapsed != 30*time.Second {
			t.Fatalf("interrupt wait = %s, want 30s", elapsed)
		}
		assertParams(t, f, "turn/interrupt", map[string]any{"threadId": "thr-1", "turnId": "turn-1"})
	})
}

func TestRunCancelBeforeStartResponse(t *testing.T) {
	f := threadFake(t)
	started, release := make(chan struct{}), make(chan struct{})
	ctx, cancel := context.WithCancel(threadContext(t))
	defer cancel()
	f.Handle("turn/start", func(json.RawMessage) (any, error) {
		close(started)
		select {
		case <-release:
		case <-ctx.Done():
		}
		return map[string]any{"turn": map[string]string{"id": "turn-1"}}, nil
	})
	f.Handle("turn/interrupt", func(json.RawMessage) (any, error) {
		completeTurn(f, "thr-1", "turn-1", "interrupted")
		return map[string]any{}, nil
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	result := make(chan error, 1)
	go func() { _, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{}); result <- err }()
	await(t, started)
	cancel()
	close(release)
	if err := await(t, result); !errors.Is(err, context.Canceled) {
		t.Fatalf("Run error = %v", err)
	}
	assertParams(t, f, "turn/interrupt", map[string]any{"threadId": "thr-1", "turnId": "turn-1"})
	assertSecondRun(t, th, f)
}

func TestRunCancelStartTimeout(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		f := threadFake(t)
		ctx, cancel := context.WithCancel(t.Context())
		defer cancel()
		release := make(chan struct{})
		defer close(release)
		f.Handle("turn/start", func(json.RawMessage) (any, error) {
			cancel()
			<-release
			return nil, nil
		})
		th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
		begin := time.Now()
		_, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{})
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Run error = %v", err)
		}
		if elapsed := time.Since(begin); elapsed != 30*time.Second {
			t.Fatalf("cancel during turn/start took %s, want 30s", elapsed)
		}
	})
}

func TestRunTwoThreadsInterleaved(t *testing.T) {
	f := threadFake(t)
	var starts atomic.Int32
	f.Handle("thread/start", func(json.RawMessage) (any, error) {
		return map[string]any{"thread": map[string]string{"id": fmt.Sprintf("thr-%d", starts.Add(1))}}, nil
	})
	ready := make(chan string, 2)
	f.Handle("turn/start", func(raw json.RawMessage) (any, error) {
		var p struct {
			ThreadID string `json:"threadId"`
		}
		if err := json.Unmarshal(raw, &p); err != nil {
			return nil, err
		}
		id := strings.TrimPrefix(p.ThreadID, "thr-")
		f.Notify("turn/started", map[string]any{"threadId": p.ThreadID, "turn": map[string]string{"id": "turn-" + id}})
		ready <- id
		return map[string]any{"turn": map[string]string{"id": "turn-" + id}}, nil
	})
	p := fakePool(t, f)
	threads := []*thread{mustStartThread(t, p, provider.ThreadOptions{}), mustStartThread(t, p, provider.ThreadOptions{})}
	type result struct {
		text      string
		err       error
		events    []map[string]any
		approvals []provider.ApprovalRequest
	}
	results := []chan result{make(chan result, 1), make(chan result, 1)}
	ctx := threadContext(t)
	for i, th := range threads {
		go func() {
			var r result
			r.text, r.err = th.Run(ctx, "hi", "", provider.ThreadCallbacks{Emit: func(e map[string]any) { r.events = append(r.events, e) }, Approve: func(a provider.ApprovalRequest) bool { r.approvals = append(r.approvals, a); return true }})
			results[i] <- r
		}()
	}
	await(t, ready)
	await(t, ready)
	for _, id := range []string{"1", "2"} {
		f.Notify("item/agentMessage/delta", map[string]string{"threadId": "thr-" + id, "turnId": "turn-" + id, "delta": id})
	}
	for _, id := range []string{"2", "1"} {
		reply := make(chan json.RawMessage, 1)
		go func() {
			reply <- f.Request("item/commandExecution/requestApproval", map[string]string{"threadId": "thr-" + id, "turnId": "turn-" + id, "command": "echo " + id})
		}()
		assertDecision(t, await(t, reply), "accept")
	}
	for _, id := range []string{"2", "1"} {
		agentMessage(f, "thr-"+id, "turn-"+id, "final_answer", id)
	}
	for _, id := range []string{"1", "2"} {
		completeTurn(f, "thr-"+id, "turn-"+id, "completed")
	}
	for i, ch := range results {
		r, id := await(t, ch), fmt.Sprint(i+1)
		if r.err != nil || r.text != id {
			t.Fatalf("thread %s Run = %q, %v", id, r.text, r.err)
		}
		wantEvents := []map[string]any{{"type": "task_started"}, {"type": "agent_message_delta", "delta": id}, {"type": "agent_message", "message": id}, {"type": "task_complete"}}
		if !reflect.DeepEqual(r.events, wantEvents) {
			t.Fatalf("thread %s events = %#v", id, r.events)
		}
		if !reflect.DeepEqual(r.approvals, []provider.ApprovalRequest{{Tool: "shell", Command: "echo " + id}}) {
			t.Fatalf("thread %s approvals = %+v", id, r.approvals)
		}
	}
}

func TestRunCrashThenResume(t *testing.T) {
	for _, duringStart := range []bool{false, true} {
		t.Run(fmt.Sprint(duringStart), func(t *testing.T) {
			f1, f2 := threadFake(t), threadFake(t)
			p := fakePool(t, f1, f2)
			scriptTurn(f1, "thr-1", "turn-1", func() { completeTurn(f1, "thr-1", "turn-1", "completed") })
			th := mustStartThread(t, p, provider.ThreadOptions{})
			if _, err := th.Run(threadContext(t), "first", "", provider.ThreadCallbacks{}); err != nil {
				t.Fatal(err)
			}
			if duringStart {
				f1.Handle("turn/start", func(json.RawMessage) (any, error) { f1.Crash(); return nil, nil })
			} else {
				scriptTurn(f1, "thr-1", "turn-2", func() {})
			}
			_, err := th.Run(threadContext(t), "crash", "", provider.ThreadCallbacks{Emit: func(e map[string]any) {
				if e["type"] == "task_started" {
					f1.Crash()
				}
			}})
			if err == nil || !strings.HasPrefix(err.Error(), "codex app-server exited:") || !strings.Contains(err.Error(), "boom") {
				t.Fatalf("crash error = %v", err)
			}
			var resumed atomic.Bool
			f2.Handle("thread/resume", func(json.RawMessage) (any, error) {
				resumed.Store(true)
				return map[string]any{"thread": map[string]string{"id": "thr-1"}}, nil
			})
			f2.Handle("turn/start", func(json.RawMessage) (any, error) {
				if !resumed.Load() {
					return nil, errors.New("turn/start before thread/resume")
				}
				agentMessage(f2, "thr-1", "turn-3", "final_answer", "resumed")
				completeTurn(f2, "thr-1", "turn-3", "completed")
				return map[string]any{"turn": map[string]string{"id": "turn-3"}}, nil
			})
			text, err := th.Run(threadContext(t), "again", "", provider.ThreadCallbacks{})
			if err != nil || text != "resumed" {
				t.Fatalf("resumed Run = %q, %v", text, err)
			}
			assertParams(t, f2, "thread/resume", map[string]any{"threadId": "thr-1"})
			if got := f2.Received("thread/start"); len(got) != 0 {
				t.Fatalf("resume sent thread/start: %s", got)
			}
		})
	}
}

func TestRunLostBeforeFirstTurn(t *testing.T) {
	f1, f2 := threadFake(t), threadFake(t)
	p := fakePool(t, f1, f2)
	th := mustStartThread(t, p, provider.ThreadOptions{})
	c, err := p.acquire(threadContext(t))
	if err != nil {
		t.Fatal(err)
	}
	f1.Crash()
	await(t, c.client.Done())
	_, err = th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{})
	if err == nil || err.Error() != "codex session lost; start a new session" {
		t.Fatalf("Run error = %v", err)
	}
	if len(f2.Received("thread/resume")) != 0 || len(f2.Received("turn/start")) != 0 {
		t.Fatal("lost thread sent resume or turn/start")
	}
}

func TestCloseUnsubscribes(t *testing.T) {
	for _, dead := range []bool{false, true} {
		t.Run(fmt.Sprint(dead), func(t *testing.T) {
			f := threadFake(t)
			p := fakePool(t, f)
			th := mustStartThread(t, p, provider.ThreadOptions{})
			c, err := p.acquire(threadContext(t))
			if err != nil {
				t.Fatal(err)
			}
			sub := c.client.Subscribe("thr-1")
			if dead {
				f.Crash()
				await(t, c.client.Done())
			}
			done := make(chan struct{})
			go func() { th.Close(); th.Close(); close(done) }()
			await(t, done)
			if !dead {
				assertParams(t, f, "thread/unsubscribe", map[string]any{"threadId": "thr-1"})
			}
			if _, err := sub.Next(threadContext(t)); err == nil {
				t.Fatal("subscription still open")
			}
		})
	}
}

func TestRunAfterClose(t *testing.T) {
	f := threadFake(t)
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	th.Close()
	result := make(chan error, 1)
	go func() { _, err := th.Run(context.Background(), "hi", "", provider.ThreadCallbacks{}); result <- err }()
	select {
	case err := <-result:
		if err == nil || err.Error() != "codex thread closed; start a new session" {
			t.Fatalf("Run error = %v", err)
		}
	case <-time.After(100 * time.Millisecond):
		t.Fatal("Run after Close did not return within 100ms")
	}
	if got := f.Received("turn/start"); len(got) != 0 {
		t.Fatalf("Run after Close sent turn/start: %s", got)
	}
}

// Helpers deliberately use only the fake's public, in-memory transport API.
type closeFunc func() error

func (f closeFunc) Close() error { return f() }

func threadFake(t *testing.T) *testutil.FakeCodex {
	t.Helper()
	f := testutil.NewFakeCodex(t)
	f.Handle("thread/start", func(json.RawMessage) (any, error) {
		return map[string]any{"thread": map[string]string{"id": "thr-1"}}, nil
	})
	f.Handle("thread/unsubscribe", func(json.RawMessage) (any, error) { return map[string]any{}, nil })
	return f
}

func fakePool(t *testing.T, fakes ...*testutil.FakeCodex) *pool {
	t.Helper()
	var mu sync.Mutex
	var n int
	return newPool(func(context.Context) (*Client, io.Closer, func() string, error) {
		mu.Lock()
		defer mu.Unlock()
		if n == len(fakes) {
			return nil, nil, nil, errors.New("unexpected dial")
		}
		f := fakes[n]
		n++
		r, w := f.ClientSide()
		c := NewClient(r, w)
		t.Cleanup(func() { f.Crash(); await(t, c.Done()) })
		return c, closeFunc(func() error { f.Crash(); return nil }), func() string { return "boom" }, nil
	})
}

func mustStartThread(t *testing.T, p *pool, opts provider.ThreadOptions) *thread {
	t.Helper()
	th, err := startThread(threadContext(t), p, opts)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(th.Close)
	return th
}

func scriptTurn(f *testutil.FakeCodex, threadID, turnID string, script func()) {
	f.Handle("turn/start", func(json.RawMessage) (any, error) {
		// Notifications preceding the RPC response must not be lost.
		f.Notify("turn/started", map[string]any{"threadId": threadID, "turn": map[string]string{"id": turnID}})
		go script() // Server requests must not hold up the turn/start response.
		return map[string]any{"turn": map[string]string{"id": turnID}}, nil
	})
}

func agentMessage(f *testutil.FakeCodex, threadID, turnID, phase, text string) {
	f.Notify("item/completed", map[string]any{"threadId": threadID, "turnId": turnID, "item": map[string]string{"type": "agentMessage", "phase": phase, "text": text}})
}

func completeTurn(f *testutil.FakeCodex, threadID, turnID, status string) {
	f.Notify("turn/completed", map[string]any{"threadId": threadID, "turn": map[string]string{"id": turnID, "status": status}})
}

func assertSecondRun(t *testing.T, th *thread, f *testutil.FakeCodex) {
	t.Helper()
	scriptTurn(f, "thr-1", "turn-2", func() {
		agentMessage(f, "thr-1", "turn-2", "final_answer", "again")
		completeTurn(f, "thr-1", "turn-2", "completed")
	})
	text, err := th.Run(threadContext(t), "again", "", provider.ThreadCallbacks{})
	if err != nil || text != "again" {
		t.Fatalf("second Run = %q, %v", text, err)
	}
}

func threadContext(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(t.Context(), 5*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func await[T any](t *testing.T, ch <-chan T) T {
	t.Helper()
	select {
	case result := <-ch:
		return result
	case <-time.After(2 * time.Second):
		t.Fatal("timed out after 2s")
	}
	var zero T
	return zero
}

func assertParams(t *testing.T, f *testutil.FakeCodex, method string, want any) {
	t.Helper()
	got := f.Received(method)
	if len(got) != 1 {
		t.Fatalf("Received(%s) = %s, want one call", method, got)
	}
	assertJSON(t, got[0], want)
}

func assertJSON(t *testing.T, raw json.RawMessage, want any) {
	t.Helper()
	var got, normalized any
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(want)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(encoded, &normalized); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, normalized) {
		t.Fatalf("JSON = %s, want %s", raw, encoded)
	}
}

func assertDecision(t *testing.T, raw json.RawMessage, want string) {
	t.Helper()
	var response Message
	if err := json.Unmarshal(raw, &response); err != nil {
		t.Fatal(err)
	}
	if response.Error != nil || len(response.ID) == 0 {
		t.Fatalf("approval reply = %s", raw)
	}
	assertJSON(t, response.Result, map[string]string{"decision": want})
}

func assertRejected(t *testing.T, raw json.RawMessage) {
	t.Helper()
	var response Message
	if err := json.Unmarshal(raw, &response); err != nil {
		t.Fatal(err)
	}
	if response.Error == nil || response.Error.Code != -32601 || response.Error.Message != "not supported by subagent-mcp" || len(response.ID) == 0 {
		t.Fatalf("unsupported reply = %s", raw)
	}
}
