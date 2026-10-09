package codexappserver

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"runtime/debug"
	"sync/atomic"
	"testing"
	"testing/synctest"
	"time"
)

func TestPoolInitializeAndGenerations(t *testing.T) {
	f1, f2 := threadFake(t), threadFake(t)
	initialized := make(chan struct{}, 2)
	f1.Handle("initialized", func(json.RawMessage) (any, error) { initialized <- struct{}{}; return nil, nil })
	f2.Handle("initialized", func(json.RawMessage) (any, error) { initialized <- struct{}{}; return nil, nil })
	p := fakePool(t, f1, f2)
	ctx := threadContext(t)
	const callers = 8
	results := make(chan *conn, callers)
	errors := make(chan error, callers)
	for range callers {
		go func() { c, err := p.acquire(ctx); results <- c; errors <- err }()
	}
	var first *conn
	for range callers {
		if err := await(t, errors); err != nil {
			t.Fatal(err)
		}
		c := await(t, results)
		if first == nil {
			first = c
		}
		if c != first {
			t.Fatal("concurrent acquire dialed more than once")
		}
	}
	if first.gen != 1 || first.userAgent != "fake-codex" {
		t.Fatalf("connection gen/agent = %d, %q", first.gen, first.userAgent)
	}
	version := "dev"
	if info, ok := debug.ReadBuildInfo(); ok && info.Main.Version != "" && info.Main.Version != "(devel)" {
		version = info.Main.Version
	}
	assertParams(t, f1, "initialize", map[string]any{"clientInfo": map[string]string{"name": "subagent-mcp", "version": version}})
	await(t, initialized)
	if got := f1.Received("initialized"); len(got) != 1 {
		t.Fatalf("initialized = %s", got)
	}
	f1.Crash()
	await(t, first.client.Done())
	second, err := p.acquire(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if second == first || second.gen != first.gen+1 || second.userAgent != "fake-codex" {
		t.Fatalf("new generation = %+v", second)
	}
	assertParams(t, f2, "initialize", map[string]any{"clientInfo": map[string]string{"name": "subagent-mcp", "version": version}})
	await(t, initialized)
	if got := f2.Received("initialized"); len(got) != 1 {
		t.Fatalf("initialized = %s", got)
	}
}

// TestShutdownClosesPooledChild pins the process-exit path: a pooled app-server
// child outlives the call that dialed it, so the shutdown func the adapter
// registers must close it instead of leaving it to keep running whatever turn it
// was handed.
func TestShutdownClosesPooledChild(t *testing.T) {
	f := threadFake(t)
	p := fakePool(t, f)
	c, err := p.acquire(threadContext(t))
	if err != nil {
		t.Fatal(err)
	}

	// Register the pool the way poolForCommand does, under a command no other
	// test uses, so Shutdown reaches exactly this child.
	const command = "codex-shutdown-test"
	poolsMu.Lock()
	pools[command] = p
	poolsMu.Unlock()
	t.Cleanup(func() {
		poolsMu.Lock()
		delete(pools, command)
		poolsMu.Unlock()
	})

	if p.current == nil {
		t.Fatal("pool has no live connection to close")
	}
	Shutdown()
	if p.current != nil {
		t.Error("Shutdown left the pooled app-server child connected")
	}
	await(t, c.client.Done())
}

// TestShutdownLeavesIdlePoolAlone pins that Shutdown is safe when the adapter
// never dialed: the exit path runs whether or not a Codex session was used.
func TestShutdownLeavesIdlePoolAlone(t *testing.T) {
	const command = "codex-idle-shutdown-test"
	p := fakePool(t)
	poolsMu.Lock()
	pools[command] = p
	poolsMu.Unlock()
	t.Cleanup(func() {
		poolsMu.Lock()
		delete(pools, command)
		poolsMu.Unlock()
	})

	Shutdown()

	if p.current != nil {
		t.Error("Shutdown dialed an idle pool")
	}
}

func TestPoolAcquireWaitingCallerCanCancel(t *testing.T) {
	f := threadFake(t)
	initializing, release := make(chan struct{}), make(chan struct{})
	defer close(release)
	f.Handle("initialize", func(json.RawMessage) (any, error) {
		close(initializing)
		<-release
		return map[string]string{"userAgent": "fake-codex"}, nil
	})
	p := fakePool(t, f)
	firstCtx, firstCancel := context.WithCancel(threadContext(t))
	defer firstCancel()
	first := make(chan error, 1)
	go func() { _, err := p.acquire(firstCtx); first <- err }()
	await(t, initializing)
	ctx, cancel := context.WithCancel(threadContext(t))
	second := make(chan error, 1)
	go func() { _, err := p.acquire(ctx); second <- err }()
	cancel()
	if err := await(t, second); !errors.Is(err, context.Canceled) {
		t.Fatalf("waiting acquire error = %v", err)
	}
	firstCancel()
	if err := await(t, first); !errors.Is(err, context.Canceled) {
		t.Fatalf("initializing acquire error = %v", err)
	}
}

func TestPoolInitializeFailureClosesConnection(t *testing.T) {
	f := threadFake(t)
	f.Handle("initialize", func(json.RawMessage) (any, error) { return nil, errors.New("initialize failed") })
	r, w := f.ClientSide()
	c := NewClient(r, w)
	var closed atomic.Bool
	p := newPool(func(context.Context) (*Client, io.Closer, func() string, error) {
		return c, closeFunc(func() error { closed.Store(true); f.Crash(); return nil }), func() string { return "" }, nil
	})
	if _, err := p.acquire(threadContext(t)); err == nil || err.Error() != "initialize failed" {
		t.Fatalf("acquire error = %v", err)
	}
	if !closed.Load() {
		t.Fatal("failed initialize did not close transport")
	}
	await(t, c.Done())
	if got := f.Received("initialized"); len(got) != 0 {
		t.Fatalf("initialized after failure = %s", got)
	}
}

func TestPoolInitializeTimeout(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		f := threadFake(t)
		release := make(chan struct{})
		defer close(release)
		f.Handle("initialize", func(json.RawMessage) (any, error) { <-release; return nil, nil })
		p := fakePool(t, f)
		begin := time.Now()
		_, err := p.acquire(t.Context())
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("acquire error = %v", err)
		}
		if elapsed := time.Since(begin); elapsed != 30*time.Second {
			t.Fatalf("initialize timeout = %s, want 30s", elapsed)
		}
	})
}
