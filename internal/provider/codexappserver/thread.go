package codexappserver

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"sync"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

var errThreadClosed = errors.New("codex thread closed; start a new session")

type thread struct {
	pool *pool
	opts provider.ThreadOptions
	id   string

	// Run is serialized by the session. This mutex protects its state from
	// Close and the subscription router, which also runs between turns.
	mu        sync.Mutex
	conn      *conn
	completed bool
	closed    bool
	active    *threadTurn
}

type threadTurn struct {
	id     string // Protected by thread.mu; unknown until turn/start responds.
	queue  *Subscription
	cancel context.CancelFunc
}

var _ provider.Thread = (*thread)(nil)

func startThread(ctx context.Context, p *pool, opts provider.ThreadOptions) (*thread, error) {
	c, err := p.acquire(ctx)
	if err != nil {
		return nil, err
	}
	policy := opts.ApprovalPolicy
	if policy == "on-failure" {
		policy = "on-request"
	}
	params := map[string]any{
		"cwd": opts.Cwd, "model": opts.Model, "sandbox": opts.Sandbox,
		"approvalPolicy": policy, "ephemeral": opts.Ephemeral,
	}
	if opts.BaseInstructions != "" {
		params["baseInstructions"] = opts.BaseInstructions
	}
	if opts.DeveloperInstructions != "" {
		params["developerInstructions"] = opts.DeveloperInstructions
	}
	var result struct {
		Thread struct{ ID string } `json:"thread"`
	}
	if err := c.client.Call(ctx, "thread/start", params, &result); err != nil {
		return nil, c.failure(err)
	}
	opts.WritableRoots = append([]string(nil), opts.WritableRoots...)
	t := &thread{pool: p, opts: opts, id: result.Thread.ID, conn: c}
	go t.route(c, c.client.Subscribe(t.id))
	return t, nil
}

// route never calls user callbacks. It rejects requests even when the thread
// is idle or Run is blocked in an approval. Each generation's router exits on
// EOF or local unsubscribe; an unbounded queue preserves early turn events.
func (t *thread) route(c *conn, sub *Subscription) {
	for {
		msg, err := sub.Next(context.Background())
		t.mu.Lock()
		active := t.active
		if err != nil {
			if t.conn == c && active != nil {
				active.queue.close(err)
			}
			t.mu.Unlock()
			return
		}
		reject := t.closed || t.conn != c || active == nil
		if len(msg.ID) != 0 && !reject {
			var p approvalParams
			reject = json.Unmarshal(msg.Params, &p) != nil || !approvalMethod(msg.Method) ||
				p.TurnID == "" || (active.id != "" && p.TurnID != active.id)
		}
		if !reject {
			active.queue.enqueue(msg)
		}
		t.mu.Unlock()
		if reject && len(msg.ID) != 0 {
			_ = rejectRequest(c, msg)
		}
	}
}

func (t *thread) Run(ctx context.Context, prompt, effort string, cb provider.ThreadCallbacks) (string, error) {
	t.mu.Lock()
	closed := t.closed
	t.mu.Unlock()
	if closed {
		return "", errThreadClosed
	}
	c, err := t.pool.acquire(ctx)
	if err != nil {
		return "", err
	}
	t.mu.Lock()
	old, completed, closed := t.conn, t.completed, t.closed
	t.mu.Unlock()
	if closed {
		return "", errThreadClosed
	}
	if c.gen != old.gen {
		if !completed {
			return "", errors.New("codex session lost; start a new session")
		}
		if err := c.client.Call(ctx, "thread/resume", map[string]string{"threadId": t.id}, nil); err != nil {
			return "", c.failure(err)
		}
	}

	// Keep the turn/start response available if cancellation races it: we need
	// its turn ID to interrupt. The same 30s cancellation budget bounds both
	// that wait and the subsequent interrupt, rather than losing the turn ID.
	runCtx, cancel := context.WithCancel(context.WithoutCancel(ctx))
	stop := context.AfterFunc(ctx, func() {
		timer := time.NewTimer(30 * time.Second)
		defer timer.Stop()
		select {
		case <-timer.C:
			cancel()
		case <-runCtx.Done():
		}
	})
	defer stop()
	defer cancel()
	active := &threadTurn{queue: &Subscription{changed: make(chan struct{})}, cancel: cancel}
	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		return "", errThreadClosed
	}
	if c != old {
		old.client.Unsubscribe(t.id)
		t.conn = c
		go t.route(c, c.client.Subscribe(t.id))
	}
	t.active = active
	t.mu.Unlock()
	defer t.finish(c, active)

	params := map[string]any{
		"threadId": t.id, "input": []map[string]string{{"type": "text", "text": prompt}}, "effort": effort,
	}
	if t.opts.Sandbox == "workspace-write" && len(t.opts.WritableRoots) != 0 {
		params["sandboxPolicy"] = map[string]any{
			"type": "workspaceWrite", "writableRoots": t.opts.WritableRoots, "networkAccess": false,
		}
	}
	var result struct {
		Turn struct{ ID string } `json:"turn"`
	}
	if err := c.client.Call(runCtx, "turn/start", params, &result); err != nil {
		return "", t.runError(ctx, c, err)
	}
	t.mu.Lock()
	active.id = result.Turn.ID
	t.mu.Unlock()

	var last, final string
	var hasFinal bool
	paths := make(map[string]string)
	for {
		if ctx.Err() != nil {
			t.interrupt(runCtx, c, active, cb)
			return "", ctx.Err()
		}
		msg, err := active.queue.Next(ctx)
		if err != nil {
			if ctx.Err() != nil {
				continue
			}
			return "", t.runError(ctx, c, err)
		}
		if len(msg.ID) != 0 {
			if err := t.approve(ctx, c, active.id, msg, paths, cb); err != nil {
				return "", t.runError(ctx, c, err)
			}
			continue
		}
		p, ok := turnNotification(msg, active.id)
		if !ok {
			continue
		}
		if msg.Method == "item/started" && p.Item.Type == "fileChange" {
			paths[p.Item.ID] = strings.Join(p.Item.paths(), ", ")
		}
		if msg.Method == "item/completed" && p.Item.Type == "agentMessage" {
			last = p.Item.Text
			if p.Item.Phase == "final_answer" {
				final, hasFinal = last, true
			}
		}
		t.emit(msg, cb)
		if msg.Method == "turn/completed" {
			t.markCompleted()
			if err := ctx.Err(); err != nil {
				return "", err
			}
			if p.Turn.Status == "failed" {
				return "", errors.New(p.Turn.Error.Message)
			}
			if hasFinal {
				return final, nil
			}
			return last, nil
		}
	}
}

func turnNotification(msg Message, turnID string) (notificationParams, bool) {
	var p notificationParams
	if json.Unmarshal(msg.Params, &p) != nil {
		return p, false
	}
	id := p.TurnID
	if msg.Method == "turn/started" || msg.Method == "turn/completed" {
		id = p.Turn.ID
		if id == "" {
			return p, false
		}
	}
	return p, id == "" || id == turnID
}

func (t *thread) emit(msg Message, cb provider.ThreadCallbacks) {
	if event, ok := mapNotification(msg.Method, msg.Params); ok && cb.Emit != nil {
		cb.Emit(event)
	}
}

func (t *thread) markCompleted() {
	t.mu.Lock()
	t.completed = true
	t.mu.Unlock()
}

type approvalParams struct {
	TurnID  string `json:"turnId"`
	ItemID  string `json:"itemId"`
	Command string `json:"command"`
	Reason  string `json:"reason"`
}

func approvalMethod(method string) bool {
	return method == "item/commandExecution/requestApproval" || method == "item/fileChange/requestApproval"
}

func rejectRequest(c *conn, msg Message) error {
	return c.client.RespondError(msg.ID, -32601, "not supported by subagent-mcp")
}

func (t *thread) approve(ctx context.Context, c *conn, turnID string, msg Message, paths map[string]string, cb provider.ThreadCallbacks) error {
	var p approvalParams
	if json.Unmarshal(msg.Params, &p) != nil || p.TurnID != turnID || !approvalMethod(msg.Method) {
		return rejectRequest(c, msg)
	}
	req := provider.ApprovalRequest{Tool: "shell", Command: p.Command, Reason: p.Reason}
	if msg.Method == "item/fileChange/requestApproval" {
		req.Tool, req.Command, req.Path = "apply_patch", "", paths[p.ItemID]
	}
	decision := "decline"
	// Approve is bound to Run's ctx by the caller, including its existing
	// elicitation timeout. Do not spawn an uninterruptible callback goroutine.
	if ctx.Err() == nil && cb.Approve != nil && cb.Approve(req) && ctx.Err() == nil {
		decision = "accept"
	}
	return c.client.Respond(msg.ID, map[string]string{"decision": decision})
}

func (t *thread) interrupt(ctx context.Context, c *conn, active *threadTurn, cb provider.ThreadCallbacks) {
	ctx, cancel := context.WithCancel(ctx)
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = c.client.Call(ctx, "turn/interrupt", map[string]string{"threadId": t.id, "turnId": active.id}, nil)
	}()
	defer func() { cancel(); <-done }()
	for {
		msg, err := active.queue.Next(ctx)
		if err != nil {
			return
		}
		if len(msg.ID) != 0 {
			// No new elicitations during interrupt; valid approvals are denied.
			_ = t.approve(ctx, c, active.id, msg, nil, provider.ThreadCallbacks{})
			continue
		}
		if _, ok := turnNotification(msg, active.id); !ok {
			continue
		}
		t.emit(msg, cb)
		if msg.Method == "turn/completed" {
			t.markCompleted()
			return
		}
	}
}

func (t *thread) runError(ctx context.Context, c *conn, err error) error {
	t.mu.Lock()
	closed := t.closed
	t.mu.Unlock()
	if closed {
		return errThreadClosed
	}
	if ctx.Err() != nil {
		return ctx.Err()
	}
	return c.failure(err)
}

func (t *thread) finish(c *conn, active *threadTurn) {
	t.mu.Lock()
	t.active = nil
	queued := active.queue.close(errors.New("turn finished"))
	t.mu.Unlock()
	for _, msg := range queued {
		if len(msg.ID) != 0 {
			_ = rejectRequest(c, msg)
		}
	}
}

func (t *thread) Close() {
	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		return
	}
	t.closed = true
	c := t.conn
	if t.active != nil {
		t.active.cancel()
	}
	t.mu.Unlock()
	c.client.Unsubscribe(t.id)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_ = c.client.Call(ctx, "thread/unsubscribe", map[string]string{"threadId": t.id}, nil)
}
