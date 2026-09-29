package agent

import (
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestManagerAcquire(t *testing.T) {
	manager := NewManager()
	session := manager.Create(Options{})

	if _, err := manager.Acquire("missing"); !errors.Is(err, ErrUnknownSession) {
		t.Fatalf("Acquire(missing) error = %v, want ErrUnknownSession", err)
	}
	got, err := manager.Acquire(session.ID)
	if err != nil || got != session {
		t.Fatalf("Acquire() = (%v, %v), want the session", got, err)
	}
	if _, err := manager.Acquire(session.ID); !errors.Is(err, ErrBusy) {
		t.Fatalf("second Acquire error = %v, want ErrBusy", err)
	}
	manager.Release(session)
	if _, err := manager.Acquire(session.ID); err != nil {
		t.Fatalf("Acquire after Release error = %v", err)
	}
}

// An expired session that a call has acquired is not evicted by the sweep that
// the next Create runs, and an evicted session can no longer be acquired.
func TestManagerAcquireAndEviction(t *testing.T) {
	manager := NewManager()
	acquired := manager.Create(Options{})
	expired := manager.Create(Options{})
	acquired.lastUsed = time.Now().Add(-25 * time.Hour)
	expired.lastUsed = time.Now().Add(-25 * time.Hour)

	if _, err := manager.Acquire(acquired.ID); err != nil {
		t.Fatal(err)
	}
	manager.Create(Options{}) // sweeps both expired sessions

	if _, ok := manager.Get(acquired.ID); !ok {
		t.Errorf("acquired session was evicted while in use")
	}
	if _, err := manager.Acquire(expired.ID); !errors.Is(err, ErrUnknownSession) {
		t.Errorf("Acquire(evicted) error = %v, want ErrUnknownSession", err)
	}
}

// The eviction sweep must not make an idle, fresh session look busy: neither
// Acquire nor Run's own TryLock on an already-fetched session may fail while
// sweeps run concurrently (the sweep used to probe every session's lock).
func TestManagerSweepDoesNotCauseFalseBusy(t *testing.T) {
	manager := NewManager()
	target := manager.Create(Options{})
	for i := 0; i < 100; i++ {
		manager.Create(Options{})
	}

	var stop atomic.Bool
	var sweeps sync.WaitGroup
	sweeps.Add(1)
	go func() {
		defer sweeps.Done()
		for !stop.Load() {
			manager.mu.Lock()
			manager.evictLocked()
			manager.mu.Unlock()
		}
	}()

	busy := 0
	for i := 0; i < 200000; i++ {
		if !target.mu.TryLock() { // what Runner.Run does
			busy++
			continue
		}
		target.mu.Unlock()
		if i%100 != 0 {
			continue
		}
		if _, err := manager.Acquire(target.ID); err != nil {
			busy++
			continue
		}
		manager.Release(target)
	}
	stop.Store(true)
	sweeps.Wait()
	if busy != 0 {
		t.Fatalf("Acquire reported ErrBusy %d times for an idle session", busy)
	}
}
