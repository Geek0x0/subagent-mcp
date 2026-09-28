package codexappserver

import (
	"context"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func TestLiveCodexAppServer(t *testing.T) {
	if os.Getenv("SUBAGENT_MCP_LIVE_CODEX") != "1" {
		t.Skip("set SUBAGENT_MCP_LIVE_CODEX=1 to run real, billed Codex calls")
	}
	p, err := provider.New("live-codex", config.Provider{API: config.APICodexAppServer}, "")
	if err != nil {
		t.Fatal(err)
	}
	a := p.(*Adapter)
	cleanupProcessPool(t, a.pool)
	ctx, cancel := context.WithTimeout(t.Context(), 2*time.Minute)
	defer cancel()
	if _, err := a.CheckAuth(ctx); err != nil {
		t.Fatal(err)
	}
	models, err := a.ListModels(ctx)
	if err != nil || len(models) == 0 {
		t.Fatalf("ListModels = %v, %v", models, err)
	}
	th, err := a.StartThread(ctx, provider.ThreadOptions{
		Model: models[0], Cwd: t.TempDir(), Sandbox: "read-only", ApprovalPolicy: "never", Ephemeral: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(th.Close)
	text, err := th.Run(ctx, "Reply with only the single number 4217.", "low", provider.ThreadCallbacks{})
	if err != nil || !strings.Contains(text, "4217") {
		t.Fatalf("Run = %q, %v", text, err)
	}
	th.Close()
	assertChildExits(t, a.pool.current, 10*time.Second)
}
