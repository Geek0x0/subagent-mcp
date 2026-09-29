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
	closedCh  chan struct{} // Closed once, by the first Close.
	active    *threadTurn
	stray     *strayTurn // A turn that may still be running after its Run returned.
}

type threadTurn struct {
	// id is protected by thread.mu. It is unknown until the turn/start response
	// or a turn/started notification, whichever arrives first; learn closes idCh.
	id        string
	idCh      chan struct{}
	done      chan struct{} // Closed by finish.
	queue     *Subscription
	cancel    context.CancelFunc
	completed bool // turn/completed was observed.
	// uncertain: turn/start ended without a server verdict (cancelled, timed out
	// or the connection failed), so the turn may exist even if its id is unknown.
	uncertain bool
	// closeTimedOut: Close gave up waiting for the id, so Run must interrupt
	// the turn itself when the turn/start response finally arrives.
	closeTimedOut bool
}

func (a *threadTurn) learn(id string) {
	if a.id == "" && id != "" {
		a.id = id
		close(a.idCh)
	}
}

// strayTurn is a turn that may still be running inside Codex although the Run
// that started it has returned (interrupt never completed, or turn/start was
// abandoned). The next Run must settle it before starting a new turn, or Codex
// would steer the new input into the old turn.
type strayTurn struct {
	id   string // Empty until a turn/started notification names it.
	gen  uint64
	idCh chan struct{} // Closed when id is learned.
	done chan struct{} // Closed when its turn/completed is observed.
}

const turnSettleBudget = 30 * time.Second

var _ provider.Thread = (*thread)(nil)

func startThread(ctx context.Context, p *pool, opts provider.ThreadOptions) (*thread, error) {
	c, err := p.acquire(ctx)
	if err != nil {
		return nil, err
	}
	params := threadSettings(opts)
	params["ephemeral"] = opts.Ephemeral
	var result struct {
		Thread struct{ ID string } `json:"thread"`
	}
	if err := c.client.Call(ctx, "thread/start", params, &result); err != nil {
		return nil, c.failure(err)
	}
	opts.WritableRoots = append([]string(nil), opts.WritableRoots...)
	t := &thread{pool: p, opts: opts, id: result.Thread.ID, conn: c, closedCh: make(chan struct{})}
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
			var queued []Message
			if t.conn == c && active != nil {
				queued = active.queue.close(err)
			}
			t.mu.Unlock()
			rejectQueued(c, queued)
			return
		}
		if t.conn == c && len(msg.ID) == 0 {
			t.observeTurnEvent(c, active, msg)
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

// observeTurnEvent lets the router learn a turn's id from turn/started before the
// turn/start response arrives, and settle a stray turn that completes between
// Runs. Callers hold t.mu.
func (t *thread) observeTurnEvent(c *conn, active *threadTurn, msg Message) {
	if msg.Method != "turn/started" && msg.Method != "turn/completed" {
		return
	}
	var p notificationParams
	if json.Unmarshal(msg.Params, &p) != nil || p.Turn.ID == "" {
		return
	}
	switch {
	case active != nil:
		if msg.Method == "turn/started" {
			active.learn(p.Turn.ID)
		}
	case t.stray != nil && t.stray.gen == c.gen:
		stray := t.stray
		if msg.Method == "turn/started" && stray.id == "" {
			stray.id = p.Turn.ID
			close(stray.idCh)
			go interruptTurn(c, t.id, stray.id, 5*time.Second)
		}
		if msg.Method == "turn/completed" && (stray.id == "" || stray.id == p.Turn.ID) {
			t.stray = nil
			close(stray.done)
		}
	}
}

// interruptTurn sends a best-effort turn/interrupt bounded by timeout.
func interruptTurn(c *conn, threadID, turnID string, timeout time.Duration) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	_ = c.client.Call(ctx, "turn/interrupt", map[string]string{"threadId": threadID, "turnId": turnID}, nil)
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
		params := threadSettings(t.opts)
		params["threadId"] = t.id
		if err := c.client.Call(ctx, "thread/resume", params, nil); err != nil {
			return "", c.failure(err)
		}
	}
	if err := t.settleStray(ctx, c); err != nil {
		return "", err
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
	active := &threadTurn{
		queue: &Subscription{changed: make(chan struct{})}, cancel: cancel,
		idCh: make(chan struct{}), done: make(chan struct{}),
	}
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
		return "", t.startFailed(ctx, c, active, err)
	}
	t.mu.Lock()
	active.learn(result.Turn.ID)
	closed, lateInterrupt := t.closed, active.closeTimedOut
	t.mu.Unlock()
	if closed {
		// Close either interrupts the turn itself or, having given up waiting
		// for the id, left that to us.
		if lateInterrupt {
			interruptTurn(c, t.id, active.id, 5*time.Second)
		}
		return "", errThreadClosed
	}

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
			t.markCompleted(active)
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

func (t *thread) markCompleted(active *threadTurn) {
	t.mu.Lock()
	t.completed = true
	active.completed = true
	t.mu.Unlock()
}

type approvalParams struct {
	TurnID                 string `json:"turnId"`
	ItemID                 string `json:"itemId"`
	Command                string `json:"command"`
	Reason                 string `json:"reason"`
	NetworkApprovalContext struct {
		Host     string `json:"host"`
		Protocol string `json:"protocol"`
	} `json:"networkApprovalContext"`
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
	if p.Command == "" {
		p.Command = p.NetworkApprovalContext.Host
	}
	req := provider.ApprovalRequest{Tool: "shell", Command: p.Command, Reason: p.Reason}
	if msg.Method == "item/fileChange/requestApproval" {
		req.Tool, req.Command, req.Path = "apply_patch", "", paths[p.ItemID]
	}
	decision := "decline"
	// Approve gets a ctx derived from Run's ctx, so the caller's cancellation
	// and elicitation timeout still apply, extended to end when the child
	// dies or this thread closes: a pending approval must not outlive either.
	// Do not spawn an uninterruptible callback goroutine.
	if cb.Approve != nil && ctx.Err() == nil {
		approveCtx, cancel := context.WithCancel(ctx)
		gone := make(chan struct{})
		go func() {
			defer close(gone)
			select {
			case <-c.client.Done():
			case <-t.closedCh:
			case <-approveCtx.Done():
			}
			cancel()
		}()
		approved := cb.Approve(approveCtx, req)
		if approved && approveCtx.Err() == nil {
			decision = "accept"
		}
		cancel()
		<-gone
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
			t.markCompleted(active)
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

// startFailed handles a failed turn/start. Unless the server itself refused the
// turn, the turn may exist: if its id is already known (turn/started arrived) and
// the caller is gone, interrupt it now; the rest is left to settleStray.
func (t *thread) startFailed(ctx context.Context, c *conn, active *threadTurn, err error) error {
	var rpcErr *RPCError
	t.mu.Lock()
	if !errors.As(err, &rpcErr) {
		active.uncertain = true
	}
	id, closed := active.id, t.closed
	t.mu.Unlock()
	if id != "" && !closed && ctx.Err() != nil {
		interruptTurn(c, t.id, id, 5*time.Second)
	}
	return t.runError(ctx, c, err)
}

// settleStray makes sure no earlier turn of this thread is still running: it
// interrupts the remembered turn and waits (within one shared budget) for its
// turn/completed. It fails rather than let Codex steer a new prompt into a turn
// that is still executing.
func (t *thread) settleStray(ctx context.Context, c *conn) error {
	t.mu.Lock()
	stray := t.stray
	if stray != nil && stray.gen != c.gen {
		// The app-server process that ran it is gone.
		t.stray, stray = nil, nil
	}
	t.mu.Unlock()
	if stray == nil {
		return nil
	}
	ctx, cancel := context.WithTimeout(ctx, turnSettleBudget)
	defer cancel()
	wait := func(ch chan struct{}) error {
		select {
		case <-ch:
			return nil
		case <-ctx.Done():
			if err := ctx.Err(); errors.Is(err, context.DeadlineExceeded) {
				return errors.New("codex is still running the previous turn of this thread; wait for it to finish or start a new session")
			}
			return ctx.Err()
		}
	}
	select {
	case <-stray.done:
		return nil
	case <-stray.idCh:
	case <-ctx.Done():
		return wait(stray.done)
	}
	t.mu.Lock()
	id := stray.id
	t.mu.Unlock()
	// Wait for the completion while the interrupt RPC is in flight: the turn may
	// finish before, or long after, the RPC is answered.
	called := make(chan struct{})
	go func() {
		defer close(called)
		_ = c.client.Call(ctx, "turn/interrupt", map[string]string{"threadId": t.id, "turnId": id}, nil)
	}()
	defer func() { cancel(); <-called }()
	return wait(stray.done)
}

func (t *thread) finish(c *conn, active *threadTurn) {
	t.mu.Lock()
	t.active = nil
	if !active.completed && !t.closed && (active.id != "" || active.uncertain) {
		t.stray = &strayTurn{id: active.id, gen: c.gen, idCh: make(chan struct{}), done: make(chan struct{})}
		if active.id != "" {
			close(t.stray.idCh)
		}
	}
	close(active.done)
	queued := active.queue.close(errors.New("turn finished"))
	t.mu.Unlock()
	rejectQueued(c, queued)
}

func (t *thread) Close() {
	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		return
	}
	t.closed = true
	close(t.closedCh)
	c := t.conn
	active := t.active
	var turnID string
	switch {
	case active != nil:
		turnID = active.id
		if turnID != "" {
			active.cancel()
		}
	case t.stray != nil:
		turnID = t.stray.id
	}
	t.mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if active != nil && turnID == "" {
		// turn/start is still in flight. Cancelling it would discard the reply
		// that names the turn, leaving Codex running it unobserved, so wait
		// (within the Close budget) for the id and interrupt it below.
		select {
		case <-active.idCh:
			active.cancel()
		case <-active.done:
		case <-ctx.Done():
			time.AfterFunc(turnSettleBudget, active.cancel)
		}
		t.mu.Lock()
		turnID = active.id
		active.closeTimedOut = turnID == "" && ctx.Err() != nil
		t.mu.Unlock()
	}
	c.client.Unsubscribe(t.id)
	if turnID != "" && c.client.Err() == nil {
		_ = c.client.Call(ctx, "turn/interrupt", map[string]string{"threadId": t.id, "turnId": turnID}, nil)
	}
	_ = c.client.Call(ctx, "thread/unsubscribe", map[string]string{"threadId": t.id}, nil)
}

func threadSettings(opts provider.ThreadOptions) map[string]any {
	policy := opts.ApprovalPolicy
	if policy == "on-failure" {
		policy = "on-request"
	}
	params := map[string]any{
		"cwd": opts.Cwd, "model": opts.Model, "sandbox": opts.Sandbox,
		"approvalPolicy": policy,
	}
	if opts.BaseInstructions != "" {
		params["baseInstructions"] = opts.BaseInstructions
	}
	if opts.DeveloperInstructions != "" {
		params["developerInstructions"] = opts.DeveloperInstructions
	}
	return params
}

func rejectQueued(c *conn, queued []Message) {
	for _, msg := range queued {
		if len(msg.ID) != 0 {
			_ = rejectRequest(c, msg)
		}
	}
}
