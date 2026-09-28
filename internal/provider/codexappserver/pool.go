package codexappserver

import (
	"context"
	"errors"
	"fmt"
	"io"
	"runtime/debug"
	"sync"
	"time"
)

type dialer func(ctx context.Context) (client *Client, closer io.Closer, stderrTail func() string, err error)

type conn struct {
	client     *Client
	gen        uint64
	userAgent  string
	closer     io.Closer
	stderrTail func() string
}

// A pool serializes initialization, sharing one live generation among threads.
// The gate lets a waiting caller cancel without waiting for another handshake.
type pool struct {
	dial    dialer
	gate    chan struct{}
	current *conn
	gen     uint64
}

func newPool(dial dialer) *pool {
	return &pool{dial: dial, gate: make(chan struct{}, 1)}
}

func (p *pool) acquire(ctx context.Context) (*conn, error) {
	select {
	case p.gate <- struct{}{}:
		defer func() { <-p.gate }()
	case <-ctx.Done():
		return nil, ctx.Err()
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if p.current != nil {
		select {
		case <-p.current.client.Done():
			_ = p.current.closer.Close()
			p.current = nil
		default:
			return p.current, nil
		}
	}

	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	client, closer, tail, err := p.dial(ctx)
	if err != nil {
		return nil, err
	}
	c := &conn{client: client, closer: &onceCloser{closer: closer}, stderrTail: tail}
	version := "dev"
	if info, ok := debug.ReadBuildInfo(); ok && info.Main.Version != "" && info.Main.Version != "(devel)" {
		version = info.Main.Version
	}
	var initialized struct {
		UserAgent string `json:"userAgent"`
	}
	if err := client.Call(ctx, "initialize", map[string]any{
		"clientInfo": map[string]string{"name": "subagent-mcp", "version": version},
	}, &initialized); err != nil {
		err = c.failure(err)
		_ = c.closer.Close()
		return nil, err
	}
	if err := client.Notify("initialized", nil); err != nil {
		err = c.failure(err)
		_ = c.closer.Close()
		return nil, err
	}
	p.gen++
	c.gen, c.userAgent = p.gen, initialized.UserAgent
	p.current = c
	return c, nil
}

// Reaping the transport before reading its stderr also lets process dialers
// finish draining stderr. Several threads may observe the same crash.
func (c *conn) failure(err error) error {
	if c.client.Err() == nil && !errors.Is(err, io.EOF) && !errors.Is(err, io.ErrClosedPipe) {
		return err
	}
	_ = c.closer.Close()
	tail := ""
	if c.stderrTail != nil {
		tail = c.stderrTail()
	}
	return fmt.Errorf("codex app-server exited: %s", tail)
}

type onceCloser struct {
	once   sync.Once
	closer io.Closer
	err    error
}

func (c *onceCloser) Close() error {
	c.once.Do(func() {
		if c.closer != nil {
			c.err = c.closer.Close()
		}
	})
	return c.err
}
