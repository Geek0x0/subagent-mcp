package codexappserver

import (
	"context"
	"fmt"
	"io"
	"os/exec"
	"sync"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/tools"
)

var (
	poolsMu sync.Mutex
	pools   = make(map[string]*pool)
)

func poolForCommand(command string) *pool {
	if command == "" {
		command = "codex"
	}
	poolsMu.Lock()
	defer poolsMu.Unlock()
	if p := pools[command]; p != nil {
		return p
	}
	p := newPool(func(ctx context.Context) (*Client, io.Closer, func() string, error) {
		return startProcess(ctx, command)
	})
	pools[command] = p
	return p
}

func startProcess(ctx context.Context, command string) (*Client, io.Closer, func() string, error) {
	if err := ctx.Err(); err != nil {
		return nil, nil, nil, err
	}
	// A shared child must outlive the call that happens to initialize it.
	cmd := exec.Command(command, "app-server")
	cmd.Env = tools.ScrubbedEnv()
	tail := &stderrBuffer{}
	cmd.Stderr = tail
	// Bound stderr draining if a descendant inherits the child's stderr pipe.
	cmd.WaitDelay = time.Second
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, nil, nil, fmt.Errorf("codex app-server stdin: %w", err)
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		_ = stdin.Close()
		return nil, nil, nil, fmt.Errorf("codex app-server stdout: %w", err)
	}
	if err := cmd.Start(); err != nil {
		_ = stdin.Close()
		_ = stdout.Close()
		return nil, nil, nil, fmt.Errorf("start codex app-server: %w", err)
	}
	client := NewClient(stdout, processWriter{stdin})
	closer := &processCloser{cmd: cmd, stdin: stdin, stdout: stdout, done: make(chan struct{})}
	go func() {
		// Wait closes StdoutPipe, so let the reader consume all output first.
		<-client.Done()
		closer.waitErr = cmd.Wait() // Also finishes copying stderr into tail.
		close(closer.done)
	}()
	return client, closer, tail.String, nil
}

type processWriter struct{ io.Writer }

func (w processWriter) Write(p []byte) (int, error) {
	n, err := w.Writer.Write(p)
	if err != nil {
		// OS pipes report EPIPE/os.ErrClosed rather than io.ErrClosedPipe.
		// Let conn.failure recognize a dead transport even before reader EOF.
		return n, fmt.Errorf("%w: %w", io.ErrClosedPipe, err)
	}
	return n, nil
}

type processCloser struct {
	cmd     *exec.Cmd
	stdin   io.Closer
	stdout  io.Closer
	once    sync.Once
	done    chan struct{}
	waitErr error // Published by closing done.
}

func (p *processCloser) Close() error {
	p.once.Do(func() {
		_ = p.stdin.Close()
		timer := time.NewTimer(5 * time.Second)
		defer timer.Stop()
		select {
		case <-p.done:
		case <-timer.C:
			_ = p.cmd.Process.Kill()
			// Release the reader even if a descendant retained stdout.
			_ = p.stdout.Close()
			<-p.done
		}
	})
	return p.waitErr
}

const stderrTailSize = 4 * 1024

// stderrBuffer retains only the last 4 KiB, including across partial writes.
type stderrBuffer struct {
	mu  sync.Mutex
	buf []byte
}

func (b *stderrBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	n := len(p)
	if b.buf == nil {
		b.buf = make([]byte, 0, stderrTailSize)
	}
	if len(p) >= stderrTailSize {
		b.buf = append(b.buf[:0], p[len(p)-stderrTailSize:]...)
	} else {
		if overflow := len(b.buf) + len(p) - stderrTailSize; overflow > 0 {
			b.buf = b.buf[:copy(b.buf, b.buf[overflow:])]
		}
		b.buf = append(b.buf, p...)
	}
	return n, nil
}

func (b *stderrBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return string(b.buf)
}
