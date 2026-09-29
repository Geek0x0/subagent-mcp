package testutil

import (
	"os"
	"path/filepath"
	"testing"
)

// RequireLandlockEnv, when set to 1, turns every Landlock-dependent skip into a
// failure, so CI on a kernel that should support Landlock cannot silently skip
// the sandbox tests.
const RequireLandlockEnv = "SUBAGENT_MCP_REQUIRE_LANDLOCK"

// LandlockUnavailable is called with the error from sandbox.Available when it is
// non-nil: it skips the test, or fails it when RequireLandlockEnv is set.
func LandlockUnavailable(t testing.TB, err error) {
	t.Helper()
	if os.Getenv(RequireLandlockEnv) == "1" {
		t.Fatalf("landlock unavailable but %s=1: %v", RequireLandlockEnv, err)
	}
	t.Skipf("landlock unavailable: %v", err)
}

// GitAncestor reports whether dir or any parent holds a .git entry. Tests that
// need a directory outside any repository cannot get one from a temp dir on a
// machine whose temp root sits inside a checkout (or holds a stray .git).
func GitAncestor(dir string) bool {
	for current := dir; ; current = filepath.Dir(current) {
		if _, err := os.Lstat(filepath.Join(current, ".git")); err == nil {
			return true
		}
		if filepath.Dir(current) == current {
			return false
		}
	}
}

// IsolateHome points HOME at a fresh empty directory for a package's TestMain, so
// no test reads the developer's login profile, config or credentials. Call the
// returned function before exiting.
func IsolateHome() func() {
	home, err := os.MkdirTemp("", "subagent-mcp-home-")
	if err != nil {
		panic(err)
	}
	old, had := os.LookupEnv("HOME")
	os.Setenv("HOME", home)
	return func() {
		if had {
			os.Setenv("HOME", old)
		} else {
			os.Unsetenv("HOME")
		}
		os.RemoveAll(home)
	}
}
