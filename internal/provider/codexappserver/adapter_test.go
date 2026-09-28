package codexappserver

import (
	"context"
	"errors"
	"io"
	"os"
	"reflect"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
	"github.com/Geek0x0/subagent-mcp/internal/tools"
)

func TestMain(m *testing.M) {
	testutil.MaybeRunFakeCodexAppServer()
	// Race options are read at process startup. Preserve existing options,
	// but disable the exit sleep for subsequently spawned fake children.
	if err := os.Setenv("GORACE", strings.TrimSpace(os.Getenv("GORACE")+" atexit_sleep_ms=0")); err != nil {
		panic(err)
	}
	os.Exit(m.Run())
}

func TestAdapterTurnUnsupported(t *testing.T) {
	a := fakeAdapter(t)
	if a.Name() != "test-codex" {
		t.Fatalf("Name = %q", a.Name())
	}
	result, err := a.Turn(t.Context(), provider.TurnRequest{}, nil)
	if result != nil || err == nil || err.Error() != "codex-app-server runs whole threads; Turn is not supported" {
		t.Fatalf("Turn = %+v, %v", result, err)
	}
	if a.pool.current != nil {
		t.Fatal("unsupported Turn started a child")
	}
}

func TestAdapterStartAndRun(t *testing.T) {
	a := fakeAdapter(t)
	th := adapterThread(t, a, threadContext(t))
	text, err := th.Run(threadContext(t), "hi", "low", provider.ThreadCallbacks{})
	if err != nil || text != "echo: hi" {
		t.Fatalf("Run = %q, %v", text, err)
	}
}

func TestAdapterListModels(t *testing.T) {
	a := fakeAdapter(t)
	t.Setenv("SUBAGENT_FAKE_CODEX_MODELS", "a,b")
	// The process fake serves one model per page, requiring nextCursor.
	models, err := a.ListModels(threadContext(t))
	if err != nil || !reflect.DeepEqual(models, []string{"a", "b"}) {
		t.Fatalf("ListModels = %v, %v", models, err)
	}
}

func TestAdapterCheckAuth(t *testing.T) {
	t.Run("logged in", func(t *testing.T) {
		a := fakeAdapter(t)
		status, err := a.CheckAuth(threadContext(t))
		want := provider.AuthStatus{Summary: "chatgpt (plus)", Version: "codex-cli 0.156.1"}
		if err != nil || status != want {
			t.Fatalf("CheckAuth = %+v, %v; want %+v", status, err, want)
		}
	})
	t.Run("logged out", func(t *testing.T) {
		a := fakeAdapter(t)
		t.Setenv("SUBAGENT_FAKE_CODEX_LOGGED_OUT", "1")
		if _, err := a.CheckAuth(threadContext(t)); err == nil || err.Error() != "not logged in; run codex login" {
			t.Fatalf("CheckAuth error = %v", err)
		}
	})
}

func TestAdapterChildEnvScrubbed(t *testing.T) {
	a := fakeAdapter(t)
	t.Setenv("DEEPSEEK_API_KEY", "secret")
	t.Setenv("SUBAGENT_MCP_TOOL_NAME", "x")
	tools.SetScrubbedEnv([]string{"DEEPSEEK_API_KEY"}, []string{"SUBAGENT_MCP_"})
	t.Cleanup(func() { tools.SetScrubbedEnv(nil, []string{"SUBAGENT_MCP_"}) })
	th := adapterThread(t, a, threadContext(t))
	text, err := th.Run(threadContext(t), "__env__", "low", provider.ThreadCallbacks{})
	if err != nil {
		t.Fatal(err)
	}
	names := strings.Split(text, "\n")
	for _, name := range []string{"DEEPSEEK_API_KEY", "SUBAGENT_MCP_TOOL_NAME"} {
		if slices.Contains(names, name) {
			t.Errorf("child inherited %s", name)
		}
	}
	if !slices.Contains(names, "SUBAGENT_FAKE_CODEX") {
		t.Fatal("child environment is missing the fake's positive-control marker")
	}
}

func TestAdapterChildExitsOnClose(t *testing.T) {
	a := fakeAdapter(t)
	c, err := a.pool.acquire(threadContext(t))
	if err != nil {
		t.Fatal(err)
	}
	assertChildExits(t, c, 5*time.Second)
}

func TestAdapterChildKilledAfterGrace(t *testing.T) {
	a := fakeAdapter(t)
	c, err := a.pool.acquire(threadContext(t))
	if err != nil {
		t.Fatal(err)
	}
	proc := childProcess(t, c)
	// A stopped child cannot consume stdin EOF, forcing the kill fallback.
	if err := proc.cmd.Process.Signal(syscall.SIGSTOP); err != nil {
		t.Fatal(err)
	}
	started := time.Now()
	closed := make(chan error, 1)
	go func() { closed <- c.closer.Close() }()
	select {
	case err := <-closed:
		if err == nil || time.Since(started) < 5*time.Second {
			t.Fatalf("Close = %v after %s; want kill after the 5s grace period", err, time.Since(started))
		}
	case <-time.After(10 * time.Second):
		t.Fatal("Close did not kill and reap the unresponsive child")
	}
	await(t, proc.done)
	await(t, c.client.Done())
}

func TestAdapterCrashStderrTail(t *testing.T) {
	a := fakeAdapter(t)
	t.Setenv("SUBAGENT_FAKE_CODEX_CRASH", "1")
	th := adapterThread(t, a, threadContext(t))
	c := a.pool.current
	result := make(chan error, 1)
	go func() {
		_, err := th.Run(t.Context(), "hi", "low", provider.ThreadCallbacks{})
		result <- err
	}()
	select {
	case err := <-result:
		if err == nil || !strings.HasPrefix(err.Error(), "codex app-server exited:") || !strings.Contains(err.Error(), "fatal: boom") {
			t.Fatalf("Run error = %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Run hung after child crash")
	}
	proc := childProcess(t, c)
	select {
	case <-proc.done:
		if proc.waitErr == nil {
			t.Fatal("crashed child had a successful wait result")
		}
	default:
		t.Fatal("Run returned without reaping the crashed child")
	}
	await(t, c.client.Done())
}

func TestAdapterSharesPool(t *testing.T) {
	a := fakeAdapter(t)
	other, err := provider.New("other-codex", config.Provider{API: config.APICodexAppServer, Command: os.Args[0]}, "ignored")
	if err != nil {
		t.Fatal(err)
	}
	b := other.(*Adapter)
	if a.pool != b.pool {
		t.Fatal("adapters for the same command have different pools")
	}
	first := adapterThread(t, a, threadContext(t)).(*thread)
	second := adapterThread(t, b, threadContext(t)).(*thread)
	if first.conn != second.conn || first.id == second.id {
		t.Fatal("threads must have distinct IDs on the same child connection")
	}
	first.Close()
	text, err := second.Run(threadContext(t), "still here", "low", provider.ThreadCallbacks{})
	if err != nil || text != "echo: still here" {
		t.Fatalf("other thread after Close = %q, %v", text, err)
	}
}

func TestAdapterChildOutlivesStartContext(t *testing.T) {
	a := fakeAdapter(t)
	ctx, cancel := context.WithCancel(threadContext(t))
	defer cancel()
	th := adapterThread(t, a, ctx)
	c := a.pool.current
	cancel()
	// The pool also cancels its initialize context after acquiring the child.
	// Neither cancellation may terminate this shared process.
	for range 3 {
		text, err := th.Run(threadContext(t), "after cancel", "low", provider.ThreadCallbacks{})
		if err != nil || text != "echo: after cancel" {
			t.Fatalf("Run after cancel = %q, %v", text, err)
		}
	}
	if a.pool.current != c {
		t.Fatal("cancellation replaced the shared child")
	}
}

func TestAdapterCrashThenResume(t *testing.T) {
	a := fakeAdapter(t)
	th := adapterThread(t, a, threadContext(t))
	if text, err := th.Run(threadContext(t), "first", "low", provider.ThreadCallbacks{}); err != nil || text != "echo: first" {
		t.Fatalf("first Run = %q, %v", text, err)
	}
	first := a.pool.current
	proc := childProcess(t, first)
	if err := proc.cmd.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	await(t, proc.done)
	await(t, first.client.Done())
	// The next generation crashes mid-turn after resuming the saved thread.
	t.Setenv("SUBAGENT_FAKE_CODEX_CRASH", "1")
	if _, err := th.Run(threadContext(t), "crash", "low", provider.ThreadCallbacks{}); err == nil || !strings.Contains(err.Error(), "fatal: boom") {
		t.Fatalf("crashing Run error = %v", err)
	}
	crashed := a.pool.current
	await(t, childProcess(t, crashed).done)
	t.Setenv("SUBAGENT_FAKE_CODEX_CRASH", "0")
	text, err := th.Run(threadContext(t), "resumed", "low", provider.ThreadCallbacks{})
	if err != nil || text != "echo: resumed" {
		t.Fatalf("resumed Run = %q, %v", text, err)
	}
	if a.pool.current.gen != first.gen+2 {
		t.Fatalf("generation = %d, want %d", a.pool.current.gen, first.gen+2)
	}
}

func TestProcessStderrTail(t *testing.T) {
	var tail stderrBuffer
	want := ""
	for _, chunk := range []string{"first", strings.Repeat("x", 4090), "fatal: boom", strings.Repeat("y", 5000), "last"} {
		n, err := tail.Write([]byte(chunk))
		if err != nil || n != len(chunk) {
			t.Fatalf("Write = %d, %v", n, err)
		}
		want += chunk
		if len(want) > 4096 {
			want = want[len(want)-4096:]
		}
		if got := tail.String(); got != want {
			t.Fatalf("stderr tail = %q, want %q", got, want)
		}
	}
}

func TestProcessWriteFailureReportsExit(t *testing.T) {
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	if err := w.Close(); err != nil {
		t.Fatal(err)
	}
	_, err = (processWriter{w}).Write([]byte("request\n"))
	if !errors.Is(err, io.ErrClosedPipe) || !errors.Is(err, os.ErrClosed) {
		t.Fatalf("write error = %v; want a transport failure preserving the OS error", err)
	}
	// The pipe can fail before the reader observes EOF. failure must still
	// reap the child and include its stderr, not expose a bare write error.
	var closed bool
	c := &conn{
		client:     &Client{},
		closer:     closeFunc(func() error { closed = true; return nil }),
		stderrTail: func() string { return "fatal: boom" },
	}
	if got := c.failure(err); got.Error() != "codex app-server exited: fatal: boom" || !closed {
		t.Fatalf("failure = %v, closed = %v", got, closed)
	}
}

func fakeAdapter(t *testing.T) *Adapter {
	t.Helper()
	t.Setenv("SUBAGENT_FAKE_CODEX", "1")
	for _, suffix := range []string{"_LOGGED_OUT", "_MODELS", "_CRASH"} {
		t.Setenv("SUBAGENT_FAKE_CODEX"+suffix, "")
	}
	p, err := provider.New("test-codex", config.Provider{API: config.APICodexAppServer, Command: os.Args[0]}, "")
	if err != nil {
		t.Fatal(err)
	}
	a, ok := p.(*Adapter)
	if !ok {
		t.Fatalf("registered provider = %T, want *Adapter", p)
	}
	cleanupProcessPool(t, a.pool)
	return a
}

func adapterThread(t *testing.T, a *Adapter, ctx context.Context) provider.Thread {
	t.Helper()
	th, err := a.StartThread(ctx, provider.ThreadOptions{
		Model: "fake-model", Cwd: t.TempDir(), Sandbox: "read-only", ApprovalPolicy: "never",
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(th.Close)
	return th
}

func cleanupProcessPool(t *testing.T, p *pool) {
	t.Helper()
	t.Cleanup(func() {
		p.gate <- struct{}{}
		defer func() { <-p.gate }()
		if c := p.current; c != nil {
			_ = c.closer.Close() // Crash tests deliberately have a nonzero exit status.
			await(t, c.client.Done())
			p.current = nil
		}
		poolsMu.Lock()
		defer poolsMu.Unlock()
		for command, entry := range pools {
			if entry == p {
				delete(pools, command)
			}
		}
	})
}

func childProcess(t *testing.T, c *conn) *processCloser {
	t.Helper()
	return c.closer.(*onceCloser).closer.(*processCloser)
}

func assertChildExits(t *testing.T, c *conn, timeout time.Duration) {
	t.Helper()
	proc := childProcess(t, c)
	closed := make(chan error, 1)
	go func() { closed <- c.closer.Close() }()
	select {
	case err := <-closed:
		if err != nil {
			t.Fatalf("child Close/wait = %v", err)
		}
	case <-time.After(timeout):
		t.Fatalf("child did not exit within %s", timeout)
	}
	select {
	case <-proc.done:
		if proc.waitErr != nil || !proc.cmd.ProcessState.Success() {
			t.Fatalf("child wait = %v, state = %v", proc.waitErr, proc.cmd.ProcessState)
		}
	default:
		t.Fatal("Close returned before the child was reaped")
	}
	await(t, c.client.Done())
}
