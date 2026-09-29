package agent

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
	code := m.Run()
	restoreHome()
	os.Exit(code)
}
