package server

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/Geek0x0/subagent-mcp/internal/agent"
	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/repo"
	"github.com/Geek0x0/subagent-mcp/internal/rollout"

	"github.com/mark3labs/mcp-go/mcp"
	mcpserver "github.com/mark3labs/mcp-go/server"
)

const (
	maxConfiguredTurns = 100000

	// stdioWorkerPool is how many tool calls the stdio server runs at once.
	// mcp-go's own default is 5; concurrent subagent threads spend most of their
	// time waiting on a model, so the pool is sized to let a caller dispatch a
	// useful number of them at once.
	stdioWorkerPool = 32

	// Ceilings for the per-call spending limits, so a typo cannot configure a
	// budget so large it never applies. The history ceiling is typed because it
	// exceeds the int range on a 32-bit build.
	maxHistoryBytesLimit  int64 = 1 << 40 // 1 TiB
	maxTotalTokensLimit         = 1_000_000_000
	maxRunDeadlineSeconds       = 7 * 24 * 60 * 60

	// defaultToolName is the base name of the two MCP tools when no
	// WithToolName option is supplied.
	defaultToolName = "subagent"

	// requestIDMetaKey is the _meta field the before-call-tool hook uses to hand
	// the JSON-RPC request ID to the tool handlers, which cannot see it otherwise.
	requestIDMetaKey = "subagent-mcp/requestId"

	// cancelledNotificationMethod is the MCP notification clients send when they
	// abandon an in-flight request, for example when the user presses Esc.
	cancelledNotificationMethod = "notifications/cancelled"

	// progressSummaryLimit is the maximum number of runes a
	// notifications/progress message carries before it is truncated.
	progressSummaryLimit = 200
)

// progressContextKey keys the per-call MCP progress state in a tool-call
// context. The state is only attached when the request opted in with a
// progress token.
type progressContextKey struct{}

// progressState tracks how many progress notifications a call has sent.
type progressState struct {
	token mcp.ProgressToken

	mu    sync.Mutex
	count int
}

func (p *progressState) next() float64 {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.count++
	return float64(p.count)
}

// canonicalRequestID returns a stable map key for a JSON-RPC request ID so the
// before-call-tool hook and notifications/cancelled agree on the same value for
// both numeric and string IDs. JSON numbers decode as float64 in both places,
// so integral floats are keyed as integers.
func canonicalRequestID(value any) (string, bool) {
	switch v := value.(type) {
	case mcp.RequestId:
		return canonicalRequestID(v.Value())
	case string:
		return "string:" + v, true
	case float64:
		if v == float64(int64(v)) {
			return "int64:" + strconv.FormatInt(int64(v), 10), true
		}
		return "float64:" + strconv.FormatFloat(v, 'f', -1, 64), true
	case int64:
		return "int64:" + strconv.FormatInt(v, 10), true
	case int:
		return "int64:" + strconv.FormatInt(int64(v), 10), true
	default:
		return "", false
	}
}

// requestIDFromMeta returns the canonical request key stored by the
// before-call-tool hook, or false when the request has none.
func requestIDFromMeta(req mcp.CallToolRequest) (string, bool) {
	if req.Params.Meta == nil {
		return "", false
	}
	key, ok := req.Params.Meta.AdditionalFields[requestIDMetaKey].(string)
	if !ok || key == "" {
		return "", false
	}
	return key, true
}

// effortValuesText lists the accepted reasoning effort values for error messages.
func effortValuesText() string {
	return strings.Join(config.EffortValues, ", ")
}

// resolveEffort returns the requested effort value. The top-level argument wins
// over config.model_reasoning_effort; each source is validated against the
// accepted effort values.
func resolveEffort(arguments map[string]any, configMap map[string]any) (string, error) {
	requested := "high"
	if raw, present := configMap["model_reasoning_effort"]; present {
		value, ok := raw.(string)
		if !ok {
			return "", fmt.Errorf("config.model_reasoning_effort must be a string; valid values: %s", effortValuesText())
		}
		if !config.ValidEffort(value) {
			return "", fmt.Errorf("invalid config.model_reasoning_effort %q; valid values: %s", value, effortValuesText())
		}
		requested = value
	}
	if raw, present := arguments["reasoning-effort"]; present {
		value, ok := raw.(string)
		if !ok {
			return "", fmt.Errorf("reasoning-effort must be a string; valid values: %s", effortValuesText())
		}
		if value != "" {
			if !config.ValidEffort(value) {
				return "", fmt.Errorf("invalid reasoning-effort %q; valid values: %s", value, effortValuesText())
			}
			requested = value
		}
	}
	return requested, nil
}

// modelUnionSchema returns every configured provider's model ids, deduplicated
// in provider-name order, and a description grouping them by provider, for
// the model parameter of the start tool.
//
// The default is marked inline, next to the id it applies to, rather than
// announced before the list: "codex (default gpt-6-luna): gpt-6-luna, ..." read
// as though the provider had one model, and a model summarising it kept only the
// default. Marking one entry of the list keeps every id visible.
func modelUnionSchema(cfg *config.Config) (ids []string, description string) {
	seen := map[string]bool{}
	var groups []string
	for _, name := range cfg.ProviderNames() {
		p := cfg.Providers[name]
		models := make([]string, 0, len(p.Models))
		for _, model := range p.Models {
			if !seen[model.ID] {
				seen[model.ID] = true
				ids = append(ids, model.ID)
			}
			rendered := model.ID
			if model.Description != "" {
				rendered = fmt.Sprintf("%s (%s)", rendered, model.Description)
			}
			if model.ID == p.DefaultModel {
				rendered += " (default)"
			}
			models = append(models, rendered)
		}
		groups = append(groups, fmt.Sprintf("%s: %s", name, strings.Join(models, ", ")))
	}
	description = fmt.Sprintf(
		"Model to use; the available id depends on the chosen provider. By provider: %s",
		strings.Join(groups, "; "),
	)
	return ids, description
}

// effortDescription describes the accepted reasoning effort values from the one
// list that defines them, so the prose cannot drift from the enum beside it.
func effortDescription() string {
	values := config.EffortValues
	var listed string
	switch len(values) {
	case 0:
		return "Reasoning effort; defaults to high, or to config.model_reasoning_effort."
	case 1:
		listed = values[0]
	default:
		listed = strings.Join(values[:len(values)-1], ", ") + ", or " + values[len(values)-1]
	}
	return fmt.Sprintf("Reasoning effort: %s; defaults to high, or to config.model_reasoning_effort.", listed)
}

// providerDescription lists every configured provider name for the provider
// parameter of the start tool.
func providerDescription(cfg *config.Config) string {
	return fmt.Sprintf(
		"Configured provider to use for this session; required when more than one is configured. Available: %s.",
		strings.Join(cfg.ProviderNames(), ", "),
	)
}

// runBudgets holds the per-session spending limits parsed from the call's config
// object. A zero field means no limit, except the history ceiling, which falls
// back to the default: a budget that only applies when asked for would not be
// the backstop against runaway growth that it exists to be.
type runBudgets struct {
	historyBytes int64
	totalTokens  int
	runDeadline  time.Duration
}

// parseBudgets reads the spending limits from the call's config object. Unlike
// max_turns and max_nudges, which fall back to their default when the value is
// out of range, an unusable limit is an error: silently ignoring a ceiling the
// caller asked for is how a run spends more than it was allowed to.
func parseBudgets(config map[string]any) (runBudgets, error) {
	budgets := runBudgets{historyBytes: agent.DefaultMaxHistoryBytes}

	if raw, present := config["max_history_bytes"]; present {
		value, ok := integerValue(raw)
		if !ok || value < 0 || int64(value) > maxHistoryBytesLimit {
			return runBudgets{}, fmt.Errorf(
				"config.max_history_bytes must be an integer from 1 to %d (0 disables the limit)",
				maxHistoryBytesLimit,
			)
		}
		budgets.historyBytes = int64(value)
	}

	if raw, present := config["max_total_tokens"]; present {
		value, ok := integerValue(raw)
		if !ok || value < 0 || value > maxTotalTokensLimit {
			return runBudgets{}, fmt.Errorf(
				"config.max_total_tokens must be an integer from 1 to %d (0 disables the limit)",
				maxTotalTokensLimit,
			)
		}
		budgets.totalTokens = int(value)
	}

	if raw, present := config["run_deadline_seconds"]; present {
		value, ok := integerValue(raw)
		if !ok || value < 0 || value > maxRunDeadlineSeconds {
			return runBudgets{}, fmt.Errorf(
				"config.run_deadline_seconds must be an integer from 1 to %d (0 disables the deadline)",
				maxRunDeadlineSeconds,
			)
		}
		budgets.runDeadline = time.Duration(value) * time.Second
	}

	return budgets, nil
}

// integerValue reports whether a JSON-decoded config value is a whole number, so
// a float like 1.5 is rejected rather than silently truncated.
func integerValue(raw any) (int, bool) {
	value, ok := raw.(float64)
	if !ok || value != float64(int(value)) {
		return 0, false
	}
	return int(value), true
}

func parseWritableRoots(config map[string]any) ([]string, error) {
	raw, present := config["writable_roots"]
	if !present {
		return nil, nil
	}
	entries, ok := raw.([]any)
	if !ok {
		return nil, errors.New("config.writable_roots must be an array of absolute directory paths")
	}
	roots := make([]string, 0, len(entries))
	for _, entry := range entries {
		path, ok := entry.(string)
		if !ok || !filepath.IsAbs(path) {
			return nil, fmt.Errorf("config.writable_roots entry %v must be an absolute path", entry)
		}
		info, err := os.Stat(path)
		if err != nil || !info.IsDir() {
			return nil, fmt.Errorf("config.writable_roots entry %q must be an existing directory", path)
		}
		roots = append(roots, filepath.Clean(path))
	}
	return roots, nil
}

// callHandle is one in-flight tool call. A handle is a pointer so a call that
// finishes can tell whether the entry under its request key is still its own:
// a client that reuses a request ID while the first call runs must not have its
// second call's registration removed by the first call's cleanup.
type callHandle struct {
	cancel context.CancelFunc
}

type Server struct {
	mcp      *mcpserver.MCPServer
	mgr      *agent.Manager
	runner   *agent.Runner
	version  string
	toolName string
	cfg      *config.Config

	callsMu sync.Mutex
	// calls maps a JSON-RPC request key to its in-flight call, for
	// notifications/cancelled. active holds every in-flight call, including the
	// rare one whose request has no ID, so shutdown can cancel them all.
	calls  map[string]*callHandle
	active map[*callHandle]struct{}
	// closing is set by Close and by the client going away. A call that begins
	// after that is cancelled at once: mcp-go's workers can still dequeue a
	// queued tools/call while the queue drains, and such a call must not run past
	// the end of the client that asked for it.
	closing bool
	// idle is non-nil while a call is running and is closed when the last one
	// returns. A WaitGroup cannot be used instead: a call starting while Close
	// waits would be an Add concurrent with Wait at counter zero, which
	// sync.WaitGroup panics on.
	idle chan struct{}

	// serveMu guards stopServe, the cancel function of the running serve loop.
	// Close calls it so the loop stops reading a client that is going away, and
	// serveStopped remembers that Close ran first when it races the loop's start.
	serveMu      sync.Mutex
	stopServe    context.CancelFunc
	serveStopped bool

	// goneCh is closed the first time the client is seen to have gone away. The
	// serve loop watches it so it can return without waiting for a worker drain
	// that a dependency ignoring cancellation could hold open indefinitely.
	goneCh   chan struct{}
	goneOnce sync.Once

	// emitDrops counts the notifications the client's bounded channel refused,
	// and emitDropLogged makes the first one the only one that writes a log line.
	emitDrops      atomic.Uint64
	emitDropLogged atomic.Bool
}

// Option configures a Server during construction.
type Option func(*Server)

// WithToolName sets the base name of the two MCP tools. The start tool is
// registered under name and the reply tool under name+"-reply".
func WithToolName(name string) Option {
	return func(s *Server) {
		s.toolName = name
	}
}

var toolNamePattern = regexp.MustCompile(`^[A-Za-z0-9_-]{1,64}$`)

// ValidateToolName reports whether name is a valid SUBAGENT_MCP_TOOL_NAME value. A
// valid name is 1 to 64 characters from A-Z, a-z, 0-9, '_', and '-', and does
// not end with "-reply", which would collide with the generated reply tool.
func ValidateToolName(name string) error {
	if !toolNamePattern.MatchString(name) {
		return fmt.Errorf(
			"SUBAGENT_MCP_TOOL_NAME %q is invalid: the name must be 1 to 64 characters matching ^[A-Za-z0-9_-]{1,64}$",
			name,
		)
	}
	if strings.HasSuffix(name, "-reply") {
		return fmt.Errorf(
			"SUBAGENT_MCP_TOOL_NAME %q is invalid: the name must not end with \"-reply\" because the reply tool is registered as %s-reply",
			name,
			name,
		)
	}
	return nil
}

// New builds a Server. cfg holds every configured provider; which one a
// session uses is resolved per call from the provider argument.
func New(cfg *config.Config, version string, opts ...Option) *Server {
	s := &Server{
		mgr:      agent.NewManager(),
		version:  version,
		toolName: defaultToolName,
		cfg:      cfg,
		calls:    make(map[string]*callHandle),
		active:   make(map[*callHandle]struct{}),
		goneCh:   make(chan struct{}),
	}
	for _, opt := range opts {
		opt(s)
	}
	hooks := &mcpserver.Hooks{}
	hooks.AddBeforeCallTool(func(_ context.Context, id any, request *mcp.CallToolRequest) {
		key, ok := canonicalRequestID(id)
		if !ok {
			return
		}
		if request.Params.Meta == nil {
			request.Params.Meta = &mcp.Meta{}
		}
		if request.Params.Meta.AdditionalFields == nil {
			request.Params.Meta.AdditionalFields = make(map[string]any)
		}
		request.Params.Meta.AdditionalFields[requestIDMetaKey] = key
	})
	s.mcp = mcpserver.NewMCPServer(
		"subagent-mcp",
		version,
		mcpserver.WithToolCapabilities(false),
		mcpserver.WithRecovery(),
		mcpserver.WithHooks(hooks),
	)
	s.mcp.AddNotificationHandler(cancelledNotificationMethod, s.handleCancelledNotification)
	s.runner = &agent.Runner{Emitter: s, Approver: s}
	s.mcp.AddTool(startTool(s.toolName, cfg), s.handleStart)
	s.mcp.AddTool(replyTool(s.toolName), s.handleReply)
	return s
}

// ServeStdio serves MCP over stdio until the client closes its end of the pipe or
// Close stops the server. The caller owns signal handling: this ends only when
// one of those two things happens.
//
// The loop is owned here rather than delegated to mcpserver.ServeStdio because a
// client that goes away has to cancel the work it left behind. mcp-go reports a
// closed stdin as a clean end of input and then drains its worker pool, so
// without the reader below a run would keep calling providers and executing shell
// commands to completion for a client that can no longer receive the result.
//
// A Server serves one client. Once the pipe closes, or Close runs, this returns
// and every later call is cancelled as it begins; a second ServeStdio would
// serve a client whose work is already refused.
func (s *Server) ServeStdio() error {
	return s.serve(context.Background(), os.Stdin, os.Stdout)
}

// serve is ServeStdio over an explicit transport, so a test can drive the loop
// without the process's own stdin and stdout.
func (s *Server) serve(parent context.Context, stdin io.Reader, stdout io.Writer) error {
	ctx, cancel := context.WithCancel(parent)
	defer cancel()
	defer s.setServeStop(nil)
	s.setServeStop(cancel)

	stdio := mcpserver.NewStdioServer(s.mcp)
	mcpserver.WithWorkerPoolSize(stdioWorkerPool)(stdio)

	// Listen returns only after it has drained its worker pool, and a run parked
	// in a dependency that ignores cancellation holds that drain open. The loop
	// therefore runs in the background and this returns as soon as the client is
	// gone or the server is closing, leaving the caller's shutdown to bound the
	// rest rather than waiting on a drain that may never finish.
	done := make(chan error, 1)
	go func() { done <- stdio.Listen(ctx, &clientGoneReader{reader: stdin, gone: s.clientGone}, stdout) }()

	select {
	case err := <-done:
		return err
	case <-s.goneCh:
		return drainResult(done, nil)
	case <-ctx.Done():
		return drainResult(done, ctx.Err())
	}
}

// drainResult reports the error the serve loop already produced, or fallback
// while it is still draining.
func drainResult(done <-chan error, fallback error) error {
	select {
	case err := <-done:
		return err
	default:
		return fallback
	}
}

// setServeStop records how Close stops the serve loop, or forgets it when the
// loop returns. A Close that raced the loop's start has already cancelled it, so
// the loop is stopped here too rather than left reading forever.
func (s *Server) setServeStop(stop context.CancelFunc) {
	s.serveMu.Lock()
	defer s.serveMu.Unlock()
	if stop == nil {
		s.stopServe = nil
		return
	}
	s.stopServe = stop
	if s.serveStopped {
		stop()
	}
}

// stopServing ends the serve loop, if one is running, and remembers the request
// so a loop that has not started yet is ended as soon as it registers.
func (s *Server) stopServing() {
	s.serveMu.Lock()
	defer s.serveMu.Unlock()
	s.serveStopped = true
	if s.stopServe != nil {
		s.stopServe()
	}
}

// clientGone is what a closed stdin means for the work still running: every
// in-flight call is cancelled, and a call that starts from the queue while it
// drains is cancelled the moment it begins. It also releases the serve loop,
// which must not wait for that drain to finish.
func (s *Server) clientGone() {
	s.goneOnce.Do(func() { close(s.goneCh) })
	s.callsMu.Lock()
	s.closing = true
	s.callsMu.Unlock()
	s.cancelCalls()
}

// clientGoneReader reports the client closing its end of the pipe, and only then:
// a read error is the whole signal stdio gives for it.
type clientGoneReader struct {
	reader io.Reader
	gone   func()
	once   sync.Once
}

func (r *clientGoneReader) Read(p []byte) (int, error) {
	n, err := r.reader.Read(p)
	if err != nil {
		r.once.Do(r.gone)
	}
	return n, err
}

// beginCall derives a cancellable context for an in-flight tool call and records
// it under the JSON-RPC request ID the before-call-tool hook stored, so a
// notifications/cancelled message can stop it. The returned stop function
// unregisters the call and releases the context. A request with no ID is still
// counted and still cancellable by Close, it just cannot be addressed by a
// cancellation notification.
func (s *Server) beginCall(ctx context.Context, req mcp.CallToolRequest) (context.Context, context.CancelFunc) {
	ctx, cancel := context.WithCancel(ctx)
	if req.Params.Meta != nil && req.Params.Meta.ProgressToken != nil {
		ctx = context.WithValue(ctx, progressContextKey{}, &progressState{token: req.Params.Meta.ProgressToken})
	}

	handle := &callHandle{cancel: cancel}
	s.callsMu.Lock()
	if len(s.active) == 0 {
		s.idle = make(chan struct{})
	}
	s.active[handle] = struct{}{}
	key, hasKey := requestIDFromMeta(req)
	if hasKey {
		s.calls[key] = handle
	}
	// A call that begins while the server is closing is cancelled here rather
	// than in Close's snapshot, which has already been taken.
	closing := s.closing
	s.callsMu.Unlock()
	if closing {
		cancel()
	}

	return ctx, func() {
		s.callsMu.Lock()
		if hasKey && s.calls[key] == handle {
			delete(s.calls, key)
		}
		delete(s.active, handle)
		if len(s.active) == 0 && s.idle != nil {
			close(s.idle)
			s.idle = nil
		}
		s.callsMu.Unlock()
		cancel()
	}
}

// waitInFlight waits until no tool call is running, and reports whether that
// happened before ctx expired.
func (s *Server) waitInFlight(ctx context.Context) bool {
	for {
		s.callsMu.Lock()
		idle := s.idle
		s.callsMu.Unlock()
		if idle == nil {
			return true
		}
		select {
		case <-idle:
		case <-ctx.Done():
			return false
		}
	}
}

// Close shuts the server down. In-flight calls are cancelled the way a
// notifications/cancelled message cancels them, their handlers are given until
// ctx expires to unwind, and every retained session is then released — bound
// directories closed and provider threads (a Codex turn among them) interrupted.
//
// A run that outlives ctx is abandoned rather than waited for, so Close always
// returns: the sessions are left to the process exit, which is what this whole
// path exists to make possible. Close also stops the serve loop, so a client
// whose pipe is still open is no longer read from.
func (s *Server) Close(ctx context.Context) {
	// Stop reading input first: the client is going away, so there is no point
	// accepting more work, and a call already queued must not start.
	s.stopServing()
	s.callsMu.Lock()
	s.closing = true
	s.callsMu.Unlock()
	s.cancelCalls()
	if !s.waitInFlight(ctx) {
		log.Printf("shutdown: in-flight calls did not finish in time; releasing sessions anyway")
	}
	if !closeBounded(ctx, s.mgr.Close) {
		log.Printf("shutdown: a session is still busy; leaving it to process exit")
	}
	if dropped := s.emitDrops.Load(); dropped > 0 {
		log.Printf("shutdown: the client's notification channel refused %d event(s); the client was not keeping up", dropped)
	}
}

// cancelCalls cancels every in-flight tool call, so a run cannot keep calling a
// provider or running shell commands while the server is going away.
func (s *Server) cancelCalls() {
	s.callsMu.Lock()
	pending := make([]context.CancelFunc, 0, len(s.active))
	for handle := range s.active {
		pending = append(pending, handle.cancel)
	}
	s.callsMu.Unlock()
	for _, cancel := range pending {
		cancel()
	}
}

// closeBounded runs release and reports whether it returned before ctx expired.
// It exists because Manager.Close blocks on the lock of a session whose run is
// still in flight — exactly the case a shutdown has to survive.
func closeBounded(ctx context.Context, release func()) bool {
	done := make(chan struct{})
	go func() {
		defer close(done)
		release()
	}()
	select {
	case <-done:
		return true
	case <-ctx.Done():
		return false
	}
}

// handleCancelledNotification cancels the in-flight call matching the
// requestId of a notifications/cancelled message. Unknown or malformed IDs are
// ignored.
func (s *Server) handleCancelledNotification(_ context.Context, notification mcp.JSONRPCNotification) {
	raw, ok := notification.Params.AdditionalFields["requestId"]
	if !ok {
		return
	}
	key, ok := canonicalRequestID(raw)
	if !ok {
		return
	}

	s.callsMu.Lock()
	handle := s.calls[key]
	s.callsMu.Unlock()
	if handle != nil {
		handle.cancel()
	}
}

type toolOutput struct {
	ThreadID string `json:"threadId" jsonschema_description:"ID of the subagent thread that produced this result."`
	Content  string `json:"content" jsonschema_description:"Human-readable report text from the subagent."`
}

func startTool(name string, cfg *config.Config) mcp.Tool {
	modelIDs, modelDesc := modelUnionSchema(cfg)
	// The provider is only genuinely optional when there is exactly one to
	// choose: with more than one configured, a call that omits it always fails,
	// so the schema has to say it is required rather than leave it to the
	// description. Nothing about which calls succeed changes; the tool contract
	// stops under-declaring what the handler already enforces.
	providerOptions := []mcp.PropertyOption{
		mcp.Enum(cfg.ProviderNames()...),
		mcp.Description(providerDescription(cfg)),
	}
	if len(cfg.Providers) > 1 {
		providerOptions = append(providerOptions, mcp.Required())
	}
	return mcp.NewTool(
		name,
		mcp.WithDescription("Start a new subagent coding-agent thread."),
		mcp.WithString(
			"prompt",
			mcp.Required(),
			mcp.Description("Task prompt to send to the new subagent thread."),
		),
		mcp.WithString("provider", providerOptions...),
		mcp.WithString(
			"model",
			mcp.Enum(modelIDs...),
			mcp.Description(modelDesc),
		),
		mcp.WithString(
			"cwd",
			mcp.Description("Absolute path to an existing working directory; defaults to the subagent-mcp process working directory."),
		),
		mcp.WithString(
			"sandbox",
			mcp.Description("Sandbox mode: read-only, workspace-write, or danger-full-access; defaults to read-only."),
		),
		mcp.WithString(
			"approval-policy",
			mcp.Description("Approval policy: untrusted, on-request, on-failure, or never; defaults to on-request."),
		),
		mcp.WithString(
			"reasoning-effort",
			mcp.Enum(config.EffortValues...),
			mcp.Description(effortDescription()),
		),
		mcp.WithString(
			"base-instructions",
			mcp.Description("Complete replacement for the built-in base system instructions; empty or omitted uses the built-in default."),
		),
		mcp.WithString(
			"developer-instructions",
			mcp.Description("Additional system instructions appended after the selected base instructions; empty or omitted appends nothing."),
		),
		mcp.WithObject(
			"config",
			mcp.Description("loose config map; recognized keys: max_turns (number from 1 to 100000), model_reasoning_effort (same values as reasoning-effort; the top-level argument wins), writable_roots (array of absolute directory paths the shell may also write under workspace-write), max_nudges (integer 0 to 10; how many times a reply without a tool call is answered with a continue prompt, overriding the provider default), max_history_bytes (integer ceiling on the history the thread replays; default 67108864, 0 disables), max_total_tokens (integer ceiling on the tokens the thread reports across all turns; default 0, off), run_deadline_seconds (integer wall-clock ceiling on one call; default 0, off); an unusable budget value is an error, other unknown keys are silently ignored"),
		),
		mcp.WithOutputSchema[toolOutput](),
	)
}

func replyTool(baseName string) mcp.Tool {
	return mcp.NewTool(
		baseName+"-reply",
		mcp.WithDescription("Continue an existing subagent coding-agent thread."),
		mcp.WithString(
			"threadId",
			mcp.Required(),
			mcp.Description(fmt.Sprintf("Thread ID returned by a previous %s or %s-reply call.", baseName, baseName)),
		),
		mcp.WithString(
			"prompt",
			mcp.Required(),
			mcp.Description("Follow-up prompt to send to the existing subagent thread."),
		),
		mcp.WithOutputSchema[toolOutput](),
	)
}

// resolveProvider picks the provider named by the "provider" argument, or the
// sole configured provider when none is given, and constructs its adapter.
// The third return value is a ready-to-return tool-error result, non-nil
// exactly when resolution failed for any reason; callers should return it
// immediately (with a nil Go error) when non-nil.
func (s *Server) resolveProvider(arguments map[string]any) (config.Provider, provider.Provider, *mcp.CallToolResult) {
	var providerArg string
	if raw, present := arguments["provider"]; present {
		var ok bool
		providerArg, ok = raw.(string)
		if !ok {
			return config.Provider{}, nil, mcp.NewToolResultError(`argument "provider" must be a string`)
		}
	}
	var providerName string
	var providerCfg config.Provider
	switch {
	case providerArg != "":
		cfg, err := s.cfg.Provider(providerArg)
		if err != nil {
			return config.Provider{}, nil, mcp.NewToolResultError(err.Error())
		}
		providerName, providerCfg = providerArg, cfg
	case len(s.cfg.Providers) == 1:
		for name, p := range s.cfg.Providers {
			providerName, providerCfg = name, p
		}
	default:
		return config.Provider{}, nil, mcp.NewToolResultError(fmt.Sprintf(
			"provider is required when more than one is configured; available: %s",
			strings.Join(s.cfg.ProviderNames(), ", "),
		))
	}
	key, err := s.cfg.APIKeyFor(providerName)
	if err != nil {
		return config.Provider{}, nil, mcp.NewToolResultError(err.Error())
	}
	adapter, err := provider.New(providerName, providerCfg, key)
	if err != nil {
		return config.Provider{}, nil, mcp.NewToolResultError(err.Error())
	}
	return providerCfg, adapter, nil
}

func (s *Server) handleStart(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	ctx, endCall := s.beginCall(ctx, req)
	defer endCall()

	prompt, err := req.RequireString("prompt")
	if err != nil {
		return mcp.NewToolResultError("prompt is required: " + err.Error()), nil
	}
	if strings.TrimSpace(prompt) == "" {
		return mcp.NewToolResultError("prompt must not be empty or whitespace-only"), nil
	}
	arguments := req.GetArguments()

	providerCfg, selectedProvider, errResult := s.resolveProvider(arguments)
	if errResult != nil {
		return errResult, nil
	}

	sandboxValue := req.GetString("sandbox", "read-only")
	sandbox, err := policy.ParseSandbox(sandboxValue)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf(
			"invalid sandbox %q; valid values: read-only, workspace-write, danger-full-access",
			sandboxValue,
		)), nil
	}

	approvalValue := req.GetString("approval-policy", "on-request")
	approval, err := policy.ParseApprovalPolicy(approvalValue)
	if err != nil {
		return mcp.NewToolResultError(fmt.Sprintf(
			"invalid approval-policy %q; valid values: untrusted, on-request, on-failure, never",
			approvalValue,
		)), nil
	}

	var model string
	if raw, present := arguments["model"]; present {
		var ok bool
		model, ok = raw.(string)
		if !ok {
			return mcp.NewToolResultError(`argument "model" must be a string`), nil
		}
	}
	if model == "" {
		model = providerCfg.DefaultModel
	} else if !providerCfg.HasModel(model) {
		return mcp.NewToolResultError(fmt.Sprintf(
			"unknown model %q; available models: %s",
			model,
			strings.Join(providerCfg.ModelIDs(), ", "),
		)), nil
	}

	looseConfig, _ := arguments["config"].(map[string]any)
	requested, err := resolveEffort(arguments, looseConfig)
	if err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	var cwd string
	if raw, present := arguments["cwd"]; present {
		var ok bool
		cwd, ok = raw.(string)
		if !ok {
			return mcp.NewToolResultError(`argument "cwd" must be a string`), nil
		}
	}
	if cwd == "" {
		cwd, err = os.Getwd()
		if err != nil {
			return mcp.NewToolResultError("resolve default cwd: " + err.Error()), nil
		}
	}
	if !filepath.IsAbs(cwd) {
		return mcp.NewToolResultError(fmt.Sprintf("cwd must be an absolute path: %q", cwd)), nil
	}
	cwdInfo, err := os.Stat(cwd)
	if err != nil {
		if os.IsNotExist(err) {
			return mcp.NewToolResultError(fmt.Sprintf("cwd does not exist: %q", cwd)), nil
		}
		return mcp.NewToolResultError(fmt.Sprintf("inspect cwd %q: %v", cwd, err)), nil
	}
	if !cwdInfo.IsDir() {
		return mcp.NewToolResultError(fmt.Sprintf("cwd is not a directory: %q", cwd)), nil
	}

	agentProvider, isAgent := selectedProvider.(provider.Agent)
	var systemPrompt string
	if !isAgent {
		systemPrompt = agent.DefaultSystemPrompt
		if baseInstructions := req.GetString("base-instructions", ""); baseInstructions != "" {
			systemPrompt = baseInstructions
		}
		agentsMD, err := agent.LoadAgentsMD(cwd)
		if err != nil {
			return mcp.NewToolResultError(err.Error()), nil
		}
		if agentsMD != "" {
			systemPrompt += "\n\n" + agentsMD
		}
		if developerInstructions := req.GetString("developer-instructions", ""); developerInstructions != "" {
			systemPrompt += "\n\n" + developerInstructions
		}
	}

	maxTurns := 0
	if value, ok := looseConfig["max_turns"].(float64); ok && value > 0 && value <= maxConfiguredTurns {
		maxTurns = int(value)
	}

	maxNudges := providerCfg.MaxNudges
	if value, ok := looseConfig["max_nudges"].(float64); ok && value >= 0 && value <= config.MaxNudgesLimit && value == float64(int(value)) {
		maxNudges = int(value)
	}

	writableRoots, err := parseWritableRoots(looseConfig)
	if err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	budgets, err := parseBudgets(looseConfig)
	if err != nil {
		return mcp.NewToolResultError(err.Error()), nil
	}

	options := agent.Options{
		Provider:        selectedProvider,
		Model:           model,
		Cwd:             cwd,
		Sandbox:         sandbox,
		Approval:        approval,
		ReasoningEffort: requested,
		EffortSent:      providerCfg.MapEffort(requested),
		SystemPrompt:    systemPrompt,
		MaxTurns:        maxTurns,
		MaxNudges:       maxNudges,
		WritableRoots:   writableRoots,
		MaxHistoryBytes: budgets.historyBytes,
		MaxTotalTokens:  budgets.totalTokens,
		RunDeadline:     budgets.runDeadline,
	}
	if isAgent {
		options.Thread, err = agentProvider.StartThread(ctx, provider.ThreadOptions{
			Model:                 model,
			Cwd:                   cwd,
			Sandbox:               string(sandbox),
			ApprovalPolicy:        string(approval),
			BaseInstructions:      req.GetString("base-instructions", ""),
			DeveloperInstructions: req.GetString("developer-instructions", ""),
			WritableRoots:         writableRoots,
		})
		if err != nil {
			return mcp.NewToolResultError(err.Error()), nil
		}
	}

	sess := s.mgr.Create(options)
	if !isAgent {
		created := time.Now()
		recorder := rollout.Open(sess.ID, created)
		sess.AttachRollout(recorder)
		meta := map[string]any{
			"session_id":        sess.ID,
			"id":                sess.ID,
			"timestamp":         created.UTC().Format(time.RFC3339Nano),
			"cwd":               cwd,
			"originator":        "subagent-mcp",
			"cli_version":       s.version,
			"source":            "mcp",
			"model_provider":    sess.Provider().Name(),
			"base_instructions": map[string]any{"text": systemPrompt},
		}
		if resolvedCwd, err := filepath.EvalSymlinks(cwd); err == nil {
			if root, ok := repo.Root(resolvedCwd); ok {
				branch, commit := repo.Head(root)
				meta["git"] = map[string]any{"branch": branch, "commit_hash": commit}
			}
		}
		recorder.Write("session_meta", meta)
	}

	text, err := s.runner.Run(ctx, sess, prompt)
	return resultWithThreadID(sess.ID, text, err), nil
}

func (s *Server) handleReply(ctx context.Context, req mcp.CallToolRequest) (*mcp.CallToolResult, error) {
	ctx, endCall := s.beginCall(ctx, req)
	defer endCall()

	threadID, err := req.RequireString("threadId")
	if err != nil {
		return mcp.NewToolResultError("threadId is required: " + err.Error()), nil
	}
	prompt, err := req.RequireString("prompt")
	if err != nil {
		return mcp.NewToolResultError("prompt is required: " + err.Error()), nil
	}
	if strings.TrimSpace(prompt) == "" {
		return mcp.NewToolResultError("prompt must not be empty or whitespace-only"), nil
	}

	sess, err := s.mgr.Acquire(threadID)
	switch {
	case errors.Is(err, agent.ErrUnknownSession):
		return mcp.NewToolResultError("unknown threadId: " + threadID), nil
	case err != nil:
		return resultWithThreadID(
			threadID,
			"",
			fmt.Errorf("thread %s is busy with another call", threadID),
		), nil
	}
	defer s.mgr.Release(sess)

	text, err := s.runner.RunLocked(ctx, sess, prompt)
	return resultWithThreadID(threadID, text, err), nil
}

func resultWithThreadID(threadID, text string, err error) *mcp.CallToolResult {
	if err == nil {
		structured := map[string]any{"threadId": threadID, "content": text}
		return mcp.NewToolResultStructured(structured, text)
	}

	errorText := "error: " + err.Error()
	structured := map[string]any{"threadId": threadID, "content": errorText}
	result := mcp.NewToolResultStructured(structured, errorText)
	result.IsError = true
	return result
}

func (s *Server) Emit(ctx context.Context, threadID string, msg map[string]any) {
	if err := s.mcp.SendNotificationToClient(ctx, "subagent/event", map[string]any{
		"threadId": threadID,
		"msg":      msg,
	}); err != nil {
		s.noteEmitDrop("subagent/event", err)
	}

	state, ok := ctx.Value(progressContextKey{}).(*progressState)
	if !ok {
		return
	}
	event, _ := msg["type"].(string)
	summary, ok := progressSummary(event, msg)
	if !ok {
		return
	}
	if err := s.mcp.SendNotificationToClient(ctx, "notifications/progress", map[string]any{
		"progressToken": state.token,
		"progress":      state.next(),
		"message":       summary,
	}); err != nil {
		s.noteEmitDrop("notifications/progress", err)
	}
}

// noteEmitDrop records a notification the client's channel refused. mcp-go's
// channel is bounded and drops rather than blocks, so a client that cannot keep
// up loses events; Emit runs on the goroutine holding a session lock, so only
// the first drop writes a log line — a `log.Printf` per dropped delta would
// itself stall the run once stderr fills, which is what makes this worth
// counting rather than logging.
func (s *Server) noteEmitDrop(kind string, err error) {
	total := s.emitDrops.Add(1)
	if s.emitDropLogged.CompareAndSwap(false, true) {
		log.Printf("%s dropped (client cannot keep up): %v; %d dropped so far, further drops are counted, not logged", kind, err, total)
	}
}

// progressSummary returns the short notifications/progress message for a runner
// event, or false for events clients should not see progress for.
func progressSummary(event string, msg map[string]any) (string, bool) {
	switch event {
	case "task_started":
		return "started", true
	case "exec_command_begin":
		if command, _ := msg["command"].(string); command != "" {
			return truncateProgressSummary("shell: " + command), true
		}
		if paths, ok := msg["paths"].([]string); ok {
			return truncateProgressSummary("apply_patch: " + strings.Join(paths, ", ")), true
		}
		tool, _ := msg["tool"].(string)
		path, _ := msg["path"].(string)
		return truncateProgressSummary(tool + ": " + path), true
	case "agent_message":
		message, _ := msg["message"].(string)
		if firstLine, _, found := strings.Cut(message, "\n"); found {
			message = firstLine
		}
		return truncateProgressSummary("agent: " + message), true
	case "task_complete":
		return "completed", true
	case "error":
		message, _ := msg["message"].(string)
		return truncateProgressSummary("error: " + message), true
	default:
		return "", false
	}
}

// truncateProgressSummary caps a summary at progressSummaryLimit runes and
// appends an ellipsis when cut, without splitting a UTF-8 character.
func truncateProgressSummary(summary string) string {
	runes := []rune(summary)
	if len(runes) <= progressSummaryLimit {
		return summary
	}
	return string(runes[:progressSummaryLimit]) + "…"
}

func (s *Server) Approve(ctx context.Context, threadID string, req agent.ApprovalRequest) bool {
	ctx, cancel := context.WithTimeout(ctx, 5*time.Minute)
	defer cancel()

	target := req.Command
	if target == "" {
		target = req.Path
	}
	result, err := s.mcp.RequestElicitation(ctx, mcp.ElicitationRequest{
		Params: mcp.ElicitationParams{
			Message: fmt.Sprintf(
				"subagent-mcp approval request (thread %s)\ntool: %s\ntarget: %s\nreason: %s",
				threadID,
				sanitizeApprovalField(req.Tool),
				sanitizeApprovalField(target),
				sanitizeApprovalField(req.Reason),
			),
			RequestedSchema: map[string]any{
				"type":       "object",
				"properties": map[string]any{},
			},
		},
	})
	if err != nil {
		log.Printf("elicitation unavailable, denying: %v", err)
		return false
	}
	return result.Action == mcp.ElicitationResponseActionAccept
}

// approvalFieldLimit is the maximum number of bytes of one untrusted,
// model-controlled field shown in the approval message before it is truncated
// with a marker stating the original length.
const approvalFieldLimit = 512

// sanitizeApprovalField renders a model-controlled value (tool name, command
// or path, justification) for the approval prompt as a single line.
// strconv.Quote escapes newlines, carriage returns, control bytes, and ANSI
// escape sequences, so no field content can forge a header line, fake the
// layout, or hide the real payload below the fold, while a normal command or
// path stays readable verbatim inside the quotes. Values longer than
// approvalFieldLimit bytes are cut on a UTF-8 boundary and get a visible
// marker stating the original length.
func sanitizeApprovalField(value string) string {
	shown := value
	truncated := len(shown) > approvalFieldLimit
	if truncated {
		cut := approvalFieldLimit
		for cut > 0 && !utf8.RuneStart(shown[cut]) {
			cut--
		}
		shown = shown[:cut]
	}
	quoted := strconv.Quote(shown)
	if truncated {
		quoted += fmt.Sprintf(" … [truncated: original length %d bytes]", len(value))
	}
	return quoted
}
