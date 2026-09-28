package agent

import (
	"os"
	"sort"
	"sync"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/rollout"

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
	WritableRoots   []string
}

type Session struct {
	ID string

	provider        provider.Provider
	thread          provider.Thread
	model           string
	reasoningEffort string
	effortSent      string
	system          string
	cwd             string
	sandbox         policy.Sandbox
	approval        policy.ApprovalPolicy
	maxTurns        int
	writableRoots   []string
	messages        []provider.Message
	rollout         *rollout.Recorder
	turnID          string
	totalUsage      provider.Usage
	lastUsed        time.Time
	mu              sync.Mutex
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
		writableRoots:   append([]string(nil), o.WritableRoots...),
		lastUsed:        m.now(),
	}

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

// evictLocked drops idle sessions that have been unused for longer than
// sessionIdleTTL, then trims the map to leave room for one new session.
// m.mu must be held. Sessions whose mu is held are busy and never evicted.
// Returned threads must be closed after releasing m.mu.
func (m *Manager) evictLocked() []provider.Thread {
	var evicted []provider.Thread
	now := m.now()
	for id, existing := range m.sessions {
		if !existing.mu.TryLock() {
			continue
		}
		lastUsed := existing.lastUsed
		existing.mu.Unlock()
		if now.Sub(lastUsed) > sessionIdleTTL {
			delete(m.sessions, id)
			if existing.thread != nil {
				evicted = append(evicted, existing.thread)
			}
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
		if !existing.mu.TryLock() {
			continue
		}
		idle = append(idle, idleSession{id: id, lastUsed: existing.lastUsed})
		existing.mu.Unlock()
	}
	sort.Slice(idle, func(i, j int) bool {
		return idle[i].lastUsed.Before(idle[j].lastUsed)
	})
	for _, candidate := range idle {
		if len(m.sessions) < maxSessions {
			break
		}
		existing, ok := m.sessions[candidate.id]
		if !ok || !existing.mu.TryLock() {
			continue
		}
		delete(m.sessions, candidate.id)
		if existing.thread != nil {
			evicted = append(evicted, existing.thread)
		}
		existing.mu.Unlock()
	}
	return evicted
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
		roots := []string{s.cwd, "/tmp"}
		if tmp := os.Getenv("TMPDIR"); tmp != "" && tmp != "/tmp" {
			if info, err := os.Stat(tmp); err == nil && info.IsDir() {
				roots = append(roots, tmp)
			}
		}
		return append(roots, s.writableRoots...), true
	default:
		return nil, false
	}
}
