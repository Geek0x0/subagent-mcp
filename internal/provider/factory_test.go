package provider

import (
	"context"
	"strings"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
)

type nopProvider struct{ name string }

func (p nopProvider) Name() string { return p.name }
func (p nopProvider) Turn(context.Context, TurnRequest, func(string)) (*TurnResult, error) {
	return &TurnResult{}, nil
}

func TestFactory(t *testing.T) {
	Register("test-api", func(name string, _ config.Provider, _ string) (Provider, error) {
		return nopProvider{name: name}, nil
	})
	p, err := New("mine", config.Provider{API: "test-api"}, "key")
	if err != nil || p.Name() != "mine" {
		t.Fatalf("New() = %v, %v", p, err)
	}
	if _, err := New("x", config.Provider{API: "grpc"}, "key"); err == nil || !strings.Contains(err.Error(), "grpc") {
		t.Fatalf("New() unknown api error = %v", err)
	}
}

// TestShutdownRunsRegisteredFuncs pins that Shutdown releases process-wide
// resources in registration order and ignores a nil func, the way an adapter
// whose cleanup only exists on some platforms registers it.
func TestShutdownRunsRegisteredFuncs(t *testing.T) {
	before := len(shutdowns)

	var ran []string
	RegisterShutdown(func() { ran = append(ran, "first") })
	RegisterShutdown(nil)
	RegisterShutdown(func() { ran = append(ran, "second") })
	t.Cleanup(func() {
		registryMu.Lock()
		shutdowns = shutdowns[:before]
		registryMu.Unlock()
	})

	Shutdown()

	if len(ran) != 2 || ran[0] != "first" || ran[1] != "second" {
		t.Fatalf("shutdown funcs ran as %v, want [first second]", ran)
	}
}

// TestShutdownRunsEveryRegistrationOnEveryCall pins the two properties the exit
// path relies on: a registered func is released once per Shutdown call, and a
// second call neither panics nor skips the work. The exit path is reachable both
// from a signal and from the client closing, and nothing else guards it.
func TestShutdownRunsEveryRegistrationOnEveryCall(t *testing.T) {
	before := len(shutdowns)
	calls := 0
	RegisterShutdown(func() { calls++ })
	t.Cleanup(func() {
		registryMu.Lock()
		shutdowns = shutdowns[:before]
		registryMu.Unlock()
	})

	Shutdown()
	Shutdown()

	if calls != 2 {
		t.Fatalf("shutdown func ran %d times over two Shutdown calls, want 2", calls)
	}
}
