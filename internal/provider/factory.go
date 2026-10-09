package provider

import (
	"fmt"
	"sync"

	"github.com/Geek0x0/subagent-mcp/internal/config"
)

// Constructor builds an adapter for one configured provider.
type Constructor func(name string, cfg config.Provider, apiKey string) (Provider, error)

// ShutdownFunc releases the process-wide resources one adapter family owns, such
// as a shared child process. An adapter registers one from init, next to
// Register; the server calls Shutdown once as the process exits.
type ShutdownFunc func()

var (
	registryMu sync.RWMutex
	registry   = map[string]Constructor{}
	shutdowns  []ShutdownFunc
)

// Register makes an adapter available for a config api value; adapters call it from init.
func Register(api string, ctor Constructor) {
	registryMu.Lock()
	defer registryMu.Unlock()
	registry[api] = ctor
}

// RegisterShutdown makes f run when the process shuts down; adapters call it
// from init alongside Register. A nil f is ignored, so an adapter can pass a
// function that only exists on some platforms.
func RegisterShutdown(f ShutdownFunc) {
	if f == nil {
		return
	}
	registryMu.Lock()
	defer registryMu.Unlock()
	shutdowns = append(shutdowns, f)
}

// Shutdown releases every registered process-wide resource, in registration
// order. It is safe when no adapter was ever constructed: an adapter that never
// started its child has nothing to release.
func Shutdown() {
	registryMu.RLock()
	pending := append([]ShutdownFunc(nil), shutdowns...)
	registryMu.RUnlock()
	for _, f := range pending {
		f()
	}
}

// New builds the adapter for cfg.API.
func New(name string, cfg config.Provider, apiKey string) (Provider, error) {
	registryMu.RLock()
	ctor, ok := registry[cfg.API]
	registryMu.RUnlock()
	if !ok {
		return nil, fmt.Errorf("unsupported api %q (is its adapter package imported?)", cfg.API)
	}
	return ctor(name, cfg, apiKey)
}
