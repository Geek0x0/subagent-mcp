package server

import (
	"os"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func TestMain(m *testing.M) {
	sandbox.MaybeRunHelper()
	// Never read the developer's login profile or home config.
	restoreHome := testutil.IsolateHome()
	// Keep tests from writing into the developer's real ~/.codex.
	os.Setenv("SUBAGENT_MCP_ROLLOUT", "off")
	code := m.Run()
	restoreHome()
	os.Exit(code)
}
