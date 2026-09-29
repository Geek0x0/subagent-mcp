package codexappserver

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"testing/synctest"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

var wantInterrupt = map[string]any{"threadId": "thr-1", "turnId": "turn-1"}

// Close while turn/start has not answered yet must not discard the reply that
// names the turn: the turn keeps running inside Codex unless it is interrupted.
func TestCloseDuringTurnStartInterruptsOnceIdKnown(t *testing.T) {
	f := threadFake(t)
	entered, release := make(chan struct{}), make(chan struct{})
	var releaseOnce sync.Once
	releaseStart := func() { releaseOnce.Do(func() { close(release) }) }
	defer releaseStart()
	f.Handle("turn/start", func(json.RawMessage) (any, error) {
		close(entered)
		<-release
		return map[string]any{"turn": map[string]string{"id": "turn-1"}}, nil
	})
	f.Handle("turn/interrupt", func(json.RawMessage) (any, error) { return map[string]any{}, nil })
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})

	runDone := make(chan error, 1)
	go func() { _, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{}); runDone <- err }()
	await(t, entered)
	closeDone := make(chan struct{})
	go func() { th.Close(); close(closeDone) }()
	time.Sleep(50 * time.Millisecond) // Close is now waiting for the turn id.
	releaseStart()

	await(t, closeDone)
	if err := await(t, runDone); !errors.Is(err, errThreadClosed) {
		t.Fatalf("Run after Close = %v, want closed error", err)
	}
	assertParams(t, f, "turn/interrupt", wantInterrupt)
}

// The id can arrive in a turn/started notification while turn/start is still
// pending; Close must use it instead of waiting for the reply.
func TestCloseDuringTurnStartUsesTurnStartedNotification(t *testing.T) {
	f := threadFake(t)
	entered, release := make(chan struct{}), make(chan struct{})
	var releaseOnce sync.Once
	releaseStart := func() { releaseOnce.Do(func() { close(release) }) }
	defer releaseStart()
	f.Handle("turn/start", func(json.RawMessage) (any, error) {
		f.Notify("turn/started", map[string]any{"threadId": "thr-1", "turn": map[string]string{"id": "turn-1"}})
		close(entered)
		<-release
		return map[string]any{"turn": map[string]string{"id": "turn-1"}}, nil
	})
	interrupted := make(chan struct{})
	f.Handle("turn/interrupt", func(json.RawMessage) (any, error) {
		close(interrupted)
		return map[string]any{}, nil
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})

	runDone := make(chan error, 1)
	go func() { _, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{}); runDone <- err }()
	await(t, entered)
	// Let the router deliver the notification before closing.
	deadline := time.Now().Add(2 * time.Second)
	for {
		th.mu.Lock()
		known := th.active != nil && th.active.id != ""
		th.mu.Unlock()
		if known || time.Now().After(deadline) {
			break
		}
		time.Sleep(time.Millisecond)
	}
	th.Close()
	await(t, interrupted) // Sent while turn/start is still unanswered.
	releaseStart()
	if err := await(t, runDone); !errors.Is(err, errThreadClosed) {
		t.Fatalf("Run after Close = %v, want closed error", err)
	}
	assertParams(t, f, "turn/interrupt", wantInterrupt)
}

// Cancel with a turn/start reply later than the 30s budget: Run gives up, but the
// turn exists, so it is interrupted as soon as its id shows up.
func TestCancelWithLateTurnStartStillInterrupts(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		f := threadFake(t)
		ctx, cancel := context.WithCancel(t.Context())
		defer cancel()
		f.Handle("turn/start", func(json.RawMessage) (any, error) {
			cancel()
			time.Sleep(40 * time.Second)
			f.Notify("turn/started", map[string]any{"threadId": "thr-1", "turn": map[string]string{"id": "turn-1"}})
			return map[string]any{"turn": map[string]string{"id": "turn-1"}}, nil
		})
		f.Handle("turn/interrupt", func(json.RawMessage) (any, error) {
			completeTurn(f, "thr-1", "turn-1", "interrupted")
			return map[string]any{}, nil
		})
		th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
		if _, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{}); !errors.Is(err, context.Canceled) {
			t.Fatalf("Run error = %v", err)
		}
		time.Sleep(15 * time.Second) // The late reply arrives; the interrupt follows.
		synctest.Wait()
		assertParams(t, f, "turn/interrupt", wantInterrupt)

		// The completion settled the turn: the next Run starts at once.
		scriptTurn(f, "thr-1", "turn-2", func() {
			agentMessage(f, "thr-1", "turn-2", "final_answer", "again")
			completeTurn(f, "thr-1", "turn-2", "completed")
		})
		text, err := th.Run(t.Context(), "again", "", provider.ThreadCallbacks{})
		if err != nil || text != "again" {
			t.Fatalf("second Run = %q, %v", text, err)
		}
		if got := len(f.Received("turn/interrupt")); got != 1 {
			t.Fatalf("turn/interrupt calls = %d, want the single early one", got)
		}
		th.Close()
	})
}

// A turn whose interrupt never completes is remembered; the next Run interrupts
// it again and refuses to start a turn on top of it.
func TestRunAfterUnfinishedTurnRefusesToStartAnother(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		f := threadFake(t)
		scriptTurn(f, "thr-1", "turn-1", func() {})
		f.Handle("turn/interrupt", func(json.RawMessage) (any, error) { return map[string]any{}, nil })
		th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})

		ctx, cancel := context.WithCancel(t.Context())
		defer cancel()
		_, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{Emit: func(e map[string]any) {
			if e["type"] == "task_started" {
				cancel()
			}
		}})
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("first Run error = %v", err)
		}

		begin := time.Now()
		_, err = th.Run(t.Context(), "again", "", provider.ThreadCallbacks{})
		if err == nil || !strings.Contains(err.Error(), "still running the previous turn") {
			t.Fatalf("second Run error = %v, want the still-running error", err)
		}
		if elapsed := time.Since(begin); elapsed != 30*time.Second {
			t.Fatalf("second Run waited %s, want the 30s budget", elapsed)
		}
		if got := len(f.Received("turn/interrupt")); got != 2 {
			t.Fatalf("turn/interrupt calls = %d, want the cancel's and a second one", got)
		}
		if got := len(f.Received("turn/start")); got != 1 {
			t.Fatalf("turn/start calls = %d, want no new turn on a running thread", got)
		}
		th.Close()
	})
}

// The interrupt completes late (after the cancelled Run gave up): the next Run
// interrupts again, sees the completion, and proceeds.
func TestRunProceedsOnceEarlierTurnCompletesLate(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		f := threadFake(t)
		scriptTurn(f, "thr-1", "turn-1", func() {})
		var calls atomic.Int32
		f.Handle("turn/interrupt", func(json.RawMessage) (any, error) {
			if calls.Add(1) == 1 {
				go func() {
					time.Sleep(40 * time.Second)
					completeTurn(f, "thr-1", "turn-1", "interrupted")
				}()
			}
			return map[string]any{}, nil
		})
		th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})

		ctx, cancel := context.WithCancel(t.Context())
		defer cancel()
		_, err := th.Run(ctx, "hi", "", provider.ThreadCallbacks{Emit: func(e map[string]any) {
			if e["type"] == "task_started" {
				cancel()
			}
		}})
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("first Run error = %v", err)
		}

		scriptTurn(f, "thr-1", "turn-2", func() {
			agentMessage(f, "thr-1", "turn-2", "final_answer", "again")
			completeTurn(f, "thr-1", "turn-2", "completed")
		})
		text, err := th.Run(t.Context(), "again", "", provider.ThreadCallbacks{})
		if err != nil || text != "again" {
			t.Fatalf("second Run = %q, %v", text, err)
		}
		if got := len(f.Received("turn/interrupt")); got != 2 {
			t.Fatalf("turn/interrupt calls = %d, want 2", got)
		}
		th.Close()
	})
}

// A normally completed or interrupted turn leaves nothing behind.
func TestCompletedTurnLeavesNoStray(t *testing.T) {
	f := threadFake(t)
	scriptTurn(f, "thr-1", "turn-1", func() {
		agentMessage(f, "thr-1", "turn-1", "final_answer", "done")
		completeTurn(f, "thr-1", "turn-1", "completed")
	})
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	if _, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{}); err != nil {
		t.Fatal(err)
	}
	th.mu.Lock()
	stray := th.stray
	th.mu.Unlock()
	if stray != nil {
		t.Fatalf("stray = %+v after a completed turn, want none", stray)
	}
}

// Close during turn/start leaves no goroutine behind, with the connection alive
// (no leaked waiter or interrupt) and after it shuts down.
func TestCloseDuringTurnStartGoroutines(t *testing.T) {
	baseline := settleGoroutines()
	f := threadFake(t)
	p := fakePool(t, f)
	c, err := p.acquire(threadContext(t))
	if err != nil {
		t.Fatal(err)
	}
	liveBaseline := settleGoroutines()
	th := mustStartThread(t, p, provider.ThreadOptions{})
	entered, release := make(chan struct{}), make(chan struct{})
	var releaseOnce sync.Once
	releaseStart := func() { releaseOnce.Do(func() { close(release) }) }
	defer releaseStart()
	f.Handle("turn/start", func(json.RawMessage) (any, error) {
		close(entered)
		<-release
		return map[string]any{"turn": map[string]string{"id": "turn-1"}}, nil
	})
	f.Handle("turn/interrupt", func(json.RawMessage) (any, error) { return map[string]any{}, nil })
	runDone := make(chan error, 1)
	go func() { _, err := th.Run(threadContext(t), "hi", "", provider.ThreadCallbacks{}); runDone <- err }()
	await(t, entered)
	closeDone := make(chan struct{})
	go func() { th.Close(); close(closeDone) }()
	time.Sleep(50 * time.Millisecond)
	releaseStart()
	await(t, closeDone)
	if err := await(t, runDone); !errors.Is(err, errThreadClosed) {
		t.Fatalf("Run after Close = %v, want closed error", err)
	}
	assertThreadGoroutines(t, liveBaseline, "Close during turn/start with live connection")
	f.Crash()
	await(t, c.client.Done())
	assertThreadGoroutines(t, baseline, "transport shutdown")
}

// A turn/completed that reached the router but was never dequeued by Run (the
// 30s budget and the completion raced) still ends the turn: nothing is left
// running, so no stray may be recorded.
func TestQueuedCompletionLeavesNoStray(t *testing.T) {
	f := threadFake(t)
	th := mustStartThread(t, fakePool(t, f), provider.ThreadOptions{})
	th.mu.Lock()
	c := th.conn
	th.mu.Unlock()
	active := &threadTurn{
		queue: &Subscription{changed: make(chan struct{})}, cancel: func() {},
		idCh: make(chan struct{}), done: make(chan struct{}),
	}
	th.mu.Lock()
	active.learn("turn-1")
	th.active = active
	th.observeTurnEvent(c, active, Message{
		Method: "turn/completed",
		Params: json.RawMessage(`{"threadId":"thr-1","turn":{"id":"turn-1","status":"interrupted"}}`),
	})
	th.mu.Unlock()

	th.finish(c, active)

	th.mu.Lock()
	stray := th.stray
	th.mu.Unlock()
	if stray != nil {
		t.Fatalf("stray = %+v after a turn whose completion was routed, want none", stray)
	}
}
