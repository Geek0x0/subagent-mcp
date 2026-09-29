package agent

import (
	"context"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/policy"
)

// A process that outlives a sandboxed shell call can keep swapping a
// subdirectory of cwd between a real directory and a symlink to the outside
// while write_file / apply_patch run. The path check alone loses that race; the
// kernel-enforced containment of the write must not.
func TestWorkspaceWritesSurviveSubdirectorySymlinkSwaps(t *testing.T) {
	for _, tool := range []struct {
		name string
		args string
	}{
		{"write_file", `{"path":"d/escaped.txt","content":"x"}`},
		{"apply_patch", `{"patch":"*** Begin Patch\n*** Add File: d/escaped.txt\n+x\n*** End Patch"}`},
	} {
		t.Run(tool.name, func(t *testing.T) {
			root := t.TempDir()
			cwd := filepath.Join(root, "work")
			outside := filepath.Join(root, "outside")
			for _, dir := range []string{cwd, outside} {
				if err := os.Mkdir(dir, 0o755); err != nil {
					t.Fatal(err)
				}
			}
			manager := NewManager()
			t.Cleanup(manager.Close)
			s := manager.Create(Options{
				Cwd: cwd, Sandbox: policy.Sandbox("workspace-write"),
				Approval: policy.ApprovalPolicy("never"),
			})
			r := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}

			link, real := filepath.Join(cwd, "d"), filepath.Join(cwd, "d.real")
			if err := os.Mkdir(link, 0o755); err != nil {
				t.Fatal(err)
			}
			var stop atomic.Bool
			var swapper sync.WaitGroup
			swapper.Add(1)
			go func() {
				defer swapper.Done()
				for !stop.Load() {
					_ = os.Rename(link, real)
					_ = os.Symlink(outside, link)
					_ = os.Remove(link)
					_ = os.Rename(real, link)
				}
			}()

			deadline := time.Now().Add(2 * time.Second)
			for i := 0; i < 20000 && time.Now().Before(deadline); i++ {
				_, _ = r.execToolCall(context.Background(), s, toolCall("race", tool.name, tool.args))
				if _, err := os.Stat(filepath.Join(outside, "escaped.txt")); err == nil {
					stop.Store(true)
					swapper.Wait()
					t.Fatalf("%s wrote outside cwd on attempt %d", tool.name, i)
				}
			}
			stop.Store(true)
			swapper.Wait()
		})
	}
}
