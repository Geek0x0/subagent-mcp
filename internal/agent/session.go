package agent

import (
	"errors"
	"fmt"
	"os"
	"sort"
	"sync"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/rollout"
	"github.com/Geek0x0/subagent-mcp/internal/sandbox"

	"github.com/google/uuid"
)

const DefaultMaxTurns = 50

const (
	sessionIdleTTL = 24 * time.Hour
	maxSessions    = 256
)

const DefaultSystemPrompt = `You are subagent-mcp, a coding agent. You work inside a fixed working directory using four tools:

- shell: run a bash command in the working directory
- read_file: read a file (relative paths resolve against the working directory)
- write_file: create or overwrite a whole file (parent directories are created)
- apply_patch: edit files with a *** Begin Patch / *** End Patch patch; prefer it for changing existing files

Work autonomously on the task you are given: inspect what you need, make the smallest change that satisfies the request, and verify it when possible. Some calls may be denied by the sandbox policy or the user; when that happens, adapt your approach or explain the blocker instead of repeating the same call. When the task is done, reply WITHOUT any tool call: summarize what you did, list changed files, and how you verified the result.`

type Options struct {
	Provider        provider.Provider
	Thread          provider.Thread
	Model           string
	ReasoningEffort string
	EffortSent      string
	Cwd             string
	Sandbox         policy.Sandbox
	Approval        policy.ApprovalPolicy
	SystemPrompt    string
	MaxTurns        int
	// MaxNudges is how many times a run may answer a reply that has no tool call
	// with a "continue" prompt (0 disables it); see Runner.RunLocked.
	MaxNudges     int
	WritableRoots []string
}

type Session struct {
	ID string

	provider        provider.Provider
	thread          provider.Thread
	threadClosed    bool
	model           string
	reasoningEffort string
	effortSent      string
	system          string
	cwd             string
	sandbox         policy.Sandbox
	approval        policy.ApprovalPolicy
	maxTurns        int
	maxNudges       int
	writableRoots   []string
	rootPaths       []string
	boundCwd        *sandbox.Directory
	boundRoots      []*sandbox.Directory
	bindErr         error
	messages        []provider.Message
	rollout         *rollout.Recorder
	turnID          string
	totalUsage      provider.Usage
	lastUsed        time.Time
	mu              sync.Mutex

	// usedMu guards lastUsed for readers that do not hold mu: the eviction sweep
	// must judge idleness without probing mu, since a probe makes a concurrent
	// caller see an idle session as busy.
	usedMu sync.Mutex
}

type Manager struct {
	mu       sync.Mutex
	sessions map[string]*Session
	now      func() time.Time
}

func NewManager() *Manager {
	return &Manager{sessions: make(map[string]*Session), now: time.Now}
}

func (m *Manager) Create(o Options) *Session {
	if o.Model == "" {
		o.Model = "deepseek-v4-pro"
	}
	if o.ReasoningEffort == "" {
		o.ReasoningEffort = "high"
	}
	if o.MaxTurns <= 0 {
		o.MaxTurns = DefaultMaxTurns
	}
	if o.SystemPrompt == "" && o.Thread == nil {
		o.SystemPrompt = DefaultSystemPrompt
	}
	if o.EffortSent == "" {
		o.EffortSent = o.ReasoningEffort
	}

	session := &Session{
		ID:              uuid.NewString(),
		provider:        o.Provider,
		thread:          o.Thread,
		model:           o.Model,
		reasoningEffort: o.ReasoningEffort,
		effortSent:      o.EffortSent,
		system:          o.SystemPrompt,
		cwd:             o.Cwd,
		sandbox:         o.Sandbox,
		approval:        o.Approval,
		maxTurns:        o.MaxTurns,
		maxNudges:       o.MaxNudges,
		writableRoots:   append([]string(nil), o.WritableRoots...),
		lastUsed:        m.now(),
	}
	session.bindDirectories()

	m.mu.Lock()
	evicted := m.evictLocked()
	m.sessions[session.ID] = session
	m.mu.Unlock()
	for _, thread := range evicted {
		// A close can block for seconds on a backend RPC; never stall the
		// caller of Create behind an eviction sweep.
		go thread.Close()
	}

	return session
}

func (s *Session) bindDirectories() {
	if s.thread != nil || s.cwd == "" {
		return
	}
	var err error
	s.boundCwd, err = sandbox.BindDirectory(s.cwd)
	if err != nil {
		s.bindErr = fmt.Errorf("bind working directory %q: %w", s.cwd, err)
		return
	}
	if s.sandbox != policy.Sandbox("workspace-write") {
		return
	}
	paths := []string{s.cwd, "/tmp"}
	if tmp := os.Getenv("TMPDIR"); tmp != "" {
		if info, statErr := os.Stat(tmp); statErr == nil && info.IsDir() {
			paths = append(paths, tmp)
		}
	}
	paths = append(paths, s.writableRoots...)
	for _, path := range paths {
		dir := s.boundCwd
		if path != s.cwd {
			dir, err = sandbox.BindDirectory(path)
			if err != nil {
				s.bindErr = fmt.Errorf("bind writable root %q: %w", path, err)
				s.closeDirectories()
				return
			}
		}
		info, err := dir.File().Stat()
		if err != nil {
			if dir != s.boundCwd {
				_ = dir.Close()
			}
			s.bindErr = fmt.Errorf("inspect writable root %q: %w", path, err)
			s.closeDirectories()
			return
		}
		duplicate := false
		for _, existing := range s.boundRoots {
			boundInfo, err := existing.File().Stat()
			if err == nil && os.SameFile(info, boundInfo) {
				duplicate = true
				break
			}
		}
		if duplicate {
			if dir != s.boundCwd {
				_ = dir.Close()
			}
			continue
		}
		s.boundRoots = append(s.boundRoots, dir)
		s.rootPaths = append(s.rootPaths, path)
	}
}

// closeDirectories is called only while the session is unused or s.mu is held.
func (s *Session) closeDirectories() {
	for _, dir := range s.boundRoots {
		if dir != s.boundCwd {
			_ = dir.Close()
		}
	}
	if s.boundCwd != nil {
		_ = s.boundCwd.Close()
	}
	s.boundCwd = nil
	s.boundRoots = nil
}

// Close releases the session's bound directories and provider thread.
func (s *Session) Close() {
	s.mu.Lock()
	s.closeDirectories()
	s.bindErr = errors.New("session is closed")
	var thread provider.Thread
	if s.thread != nil && !s.threadClosed {
		s.threadClosed = true
		thread = s.thread
	}
	s.mu.Unlock()
	if thread != nil {
		thread.Close()
	}
}

// Close releases every session retained by the manager.
func (m *Manager) Close() {
	m.mu.Lock()
	sessions := m.sessions
	m.sessions = make(map[string]*Session)
	m.mu.Unlock()
	for _, session := range sessions {
		session.Close()
	}
}

// evictLocked drops idle sessions that have been unused for longer than
// sessionIdleTTL, then trims the map to leave room for one new session.
// m.mu must be held. Only sessions chosen for eviction have their mu probed, and
// a session whose mu is held (acquired or running) is never evicted.
// Returned threads must be closed after releasing m.mu.
func (m *Manager) evictLocked() []provider.Thread {
	var evicted []provider.Thread
	now := m.now()
	for id, existing := range m.sessions {
		if now.Sub(existing.lastUsedAt()) <= sessionIdleTTL {
			continue
		}
		if thread, ok := m.evictIfIdleLocked(id, existing); ok && thread != nil {
			evicted = append(evicted, thread)
		}
	}

	if len(m.sessions) < maxSessions {
		return evicted
	}

	type idleSession struct {
		id       string
		lastUsed time.Time
	}
	idle := make([]idleSession, 0, len(m.sessions))
	for id, existing := range m.sessions {
		idle = append(idle, idleSession{id: id, lastUsed: existing.lastUsedAt()})
	}
	sort.Slice(idle, func(i, j int) bool {
		return idle[i].lastUsed.Before(idle[j].lastUsed)
	})
	for _, candidate := range idle {
		if len(m.sessions) < maxSessions {
			break
		}
		if thread, ok := m.evictIfIdleLocked(candidate.id, m.sessions[candidate.id]); ok && thread != nil {
			evicted = append(evicted, thread)
		}
	}
	return evicted
}

// evictIfIdleLocked removes the session unless its mu is held. m.mu must be held;
// because Acquire also takes mu only under m.mu, a session cannot be handed out
// and evicted at the same time.
func (m *Manager) evictIfIdleLocked(id string, existing *Session) (provider.Thread, bool) {
	if !existing.mu.TryLock() {
		return nil, false
	}
	defer existing.mu.Unlock()
	delete(m.sessions, id)
	existing.closeDirectories()
	existing.bindErr = errors.New("session was evicted")
	if existing.thread != nil && !existing.threadClosed {
		existing.threadClosed = true
		return existing.thread, true
	}
	return nil, true
}

// ErrUnknownSession reports that Acquire found no session with the given ID
// (never created, closed, or evicted).
var ErrUnknownSession = errors.New("unknown session")

// Acquire looks a session up and takes its lock in one step, so the session
// cannot be evicted between lookup and use, and an evicted one is never handed
// out. It fails with ErrUnknownSession or, when another call holds the session,
// ErrBusy. The caller runs the session with Runner.RunLocked and then calls
// Release.
func (m *Manager) Acquire(id string) (*Session, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	session, ok := m.sessions[id]
	if !ok {
		return nil, ErrUnknownSession
	}
	if !session.mu.TryLock() {
		return nil, ErrBusy
	}
	return session, nil
}

// Release returns a session obtained from Acquire.
func (m *Manager) Release(s *Session) { s.mu.Unlock() }

func (s *Session) lastUsedAt() time.Time {
	s.usedMu.Lock()
	defer s.usedMu.Unlock()
	return s.lastUsed
}

func (s *Session) setLastUsed(t time.Time) {
	s.usedMu.Lock()
	s.lastUsed = t
	s.usedMu.Unlock()
}

func (m *Manager) Get(id string) (*Session, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()

	session, ok := m.sessions[id]
	return session, ok
}

// AttachRollout sets the recorder that receives this session's rollout lines.
func (s *Session) AttachRollout(r *rollout.Recorder) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.rollout = r
}

// Provider returns the provider.Provider instance this session was created with.
func (s *Session) Provider() provider.Provider {
	return s.provider
}

// shellWritableRoots returns the Landlock writable roots for shell calls the
// policy auto-allows, and false when such calls run without the kernel sandbox.
func (s *Session) shellWritableRoots() ([]string, bool) {
	switch s.sandbox {
	case policy.Sandbox("read-only"):
		return []string{}, true
	case policy.Sandbox("workspace-write"):
		return append([]string(nil), s.rootPaths...), true
	default:
		return nil, false
	}
}
