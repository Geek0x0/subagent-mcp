package agent

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func TestSessionCwdRenameCannotMoveSandboxBoundary(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		testutil.LandlockUnavailable(t, err)
	}
	parent, err := os.MkdirTemp(".", "bound-parent-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(parent) })
	protected, err := os.MkdirTemp(".", "protected-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(protected) })
	parent, err = filepath.Abs(parent)
	if err != nil {
		t.Fatal(err)
	}
	protected, err = filepath.Abs(protected)
	if err != nil {
		t.Fatal(err)
	}
	cwd := filepath.Join(parent, "work")
	if err := os.Mkdir(cwd, 0o755); err != nil {
		t.Fatal(err)
	}
	manager := NewManager()
	t.Cleanup(manager.Close)
	s := manager.Create(Options{
		Cwd: cwd, Sandbox: policy.Sandbox("workspace-write"),
		Approval: policy.ApprovalPolicy("never"), WritableRoots: []string{parent},
	})
	r := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	call := func(name, args string) (string, bool) {
		t.Helper()
		return r.execToolCall(context.Background(), s, toolCall("boundary", name, args))
	}
	if result, failed := call("write_file", `{"path":"before","content":"ok"}`); failed {
		t.Fatalf("write inside original cwd failed: %s", result)
	}
	if data, err := os.ReadFile(filepath.Join(cwd, "before")); err != nil || string(data) != "ok" {
		t.Fatalf("original cwd write = %q, %v", data, err)
	}
	move := fmt.Sprintf("mv %q %q && ln -s %q %q", cwd, cwd+".bak", protected, cwd)
	if result, failed := call("shell", fmt.Sprintf(`{"command":%q}`, move)); failed {
		t.Fatalf("sandboxed rename failed: %s", result)
	}
	result, failed := call("shell", `{"command":"echo pwned > f2"}`)
	if failed {
		t.Fatalf("shell in bound cwd failed after rename: %s", result)
	}
	if data, err := os.ReadFile(filepath.Join(cwd+".bak", "f2")); err != nil || string(data) != "pwned\n" {
		t.Fatalf("bound cwd shell write = %q, %v", data, err)
	}
	if _, err := os.Stat(filepath.Join(protected, "f2")); err == nil {
		t.Fatalf("sandboxed shell escaped into protected/f2: %s (failed=%v)", result, failed)
	} else if !os.IsNotExist(err) {
		t.Fatal(err)
	}
	result, failed = call("write_file", `{"path":"f3","content":"pwned"}`)
	if !failed || !strings.Contains(result, "bound directory") {
		t.Fatalf("write_file after cwd replacement = (%q, failed=%v), want bound-directory error", result, failed)
	}
	if _, err := os.Stat(filepath.Join(protected, "f3")); err == nil {
		t.Fatalf("write_file escaped into protected/f3: %s (failed=%v)", result, failed)
	} else if !os.IsNotExist(err) {
		t.Fatal(err)
	}
	result, failed = call("apply_patch", `{"patch":"*** Begin Patch\n*** Add File: f4\n+pwned\n*** End Patch"}`)
	if !failed || !strings.Contains(result, "bound directory") {
		t.Fatalf("apply_patch after cwd replacement = (%q, failed=%v), want bound-directory error", result, failed)
	}
	if _, err := os.Stat(filepath.Join(protected, "f4")); !os.IsNotExist(err) {
		t.Fatalf("apply_patch touched protected/f4: %v", err)
	}
}

func TestSessionConfiguredAndTmpdirRootsReachLandlock(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		testutil.LandlockUnavailable(t, err)
	}
	newDir := func(prefix string) string {
		t.Helper()
		path, err := os.MkdirTemp(".", prefix)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = os.RemoveAll(path) })
		absolute, err := filepath.Abs(path)
		if err != nil {
			t.Fatal(err)
		}
		return absolute
	}
	cwd, configured, tmpdir, outside := newDir("cwd-"), newDir("configured-"), newDir("tmpdir-"), newDir("outside-")
	t.Setenv("TMPDIR", tmpdir)
	manager := NewManager()
	t.Cleanup(manager.Close)
	s := manager.Create(Options{Cwd: cwd, Sandbox: policy.Sandbox("workspace-write"), Approval: policy.ApprovalPolicy("never"), WritableRoots: []string{configured}})
	r := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	for _, path := range []string{filepath.Join(configured, "ok"), filepath.Join(tmpdir, "ok")} {
		result, failed := r.execToolCall(context.Background(), s, toolCall("roots", "shell", fmt.Sprintf(`{"command":%q}`, fmt.Sprintf("echo ok > %q", path))))
		if failed {
			t.Fatalf("sandboxed write to %q failed: %s", path, result)
		}
		if data, err := os.ReadFile(path); err != nil || string(data) != "ok\n" {
			t.Fatalf("sandboxed write to %q = %q, %v", path, data, err)
		}
	}
	blocked := filepath.Join(outside, "bad")
	result, failed := r.execToolCall(context.Background(), s, toolCall("roots", "shell", fmt.Sprintf(`{"command":%q}`, fmt.Sprintf("echo bad > %q", blocked))))
	if failed || !strings.Contains(result, "exit code: 1") {
		t.Fatalf("outside sandbox write = (%q, failed=%v), want shell exit 1", result, failed)
	}
	if _, err := os.Stat(blocked); !os.IsNotExist(err) {
		t.Fatalf("outside file was written: %v", err)
	}
}

func TestSessionWritableRootReplacementCannotRedirectLandlock(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		testutil.LandlockUnavailable(t, err)
	}
	parent, err := os.MkdirTemp(".", "root-parent-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(parent) })
	protected, err := os.MkdirTemp(".", "root-protected-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(protected) })
	parent, err = filepath.Abs(parent)
	if err != nil {
		t.Fatal(err)
	}
	protected, err = filepath.Abs(protected)
	if err != nil {
		t.Fatal(err)
	}
	root := filepath.Join(parent, "writable")
	if err := os.Mkdir(root, 0o755); err != nil {
		t.Fatal(err)
	}
	manager := NewManager()
	t.Cleanup(manager.Close)
	s := manager.Create(Options{Cwd: t.TempDir(), Sandbox: policy.Sandbox("workspace-write"), Approval: policy.ApprovalPolicy("never"), WritableRoots: []string{root}})
	if err := os.Rename(root, root+".bak"); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(protected, root); err != nil {
		t.Fatal(err)
	}
	r := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	for _, tc := range []struct {
		path     string
		wantCode string
	}{
		{filepath.Join(root, "blocked"), "exit code: 1"},
		{filepath.Join(root+".bak", "allowed"), "exit code: 0"},
	} {
		result, failed := r.execToolCall(context.Background(), s, toolCall("root-replaced", "shell", fmt.Sprintf(`{"command":%q}`, fmt.Sprintf("echo ok > %q", tc.path))))
		if failed || !strings.Contains(result, tc.wantCode) {
			t.Fatalf("shell write %q = (%q, failed=%v), want %s", tc.path, result, failed, tc.wantCode)
		}
	}
	if _, err := os.Stat(filepath.Join(protected, "blocked")); !os.IsNotExist(err) {
		t.Fatalf("protected directory received a write: %v", err)
	}
	if data, err := os.ReadFile(filepath.Join(root+".bak", "allowed")); err != nil || string(data) != "ok\n" {
		t.Fatalf("held writable root write = %q, %v", data, err)
	}
}

func TestManagerBoundDirectoryHandlesClose(t *testing.T) {
	if _, err := os.Stat("/proc/self/fd"); err != nil {
		t.Skip("/proc/self/fd unavailable")
	}
	countFDs := func() int {
		t.Helper()
		entries, err := os.ReadDir("/proc/self/fd")
		if err != nil {
			t.Fatal(err)
		}
		return len(entries)
	}
	cwd := t.TempDir()
	baseline := countFDs()
	manager := NewManager()
	current := time.Now()
	manager.now = func() time.Time { return current }
	first := manager.Create(Options{Cwd: cwd, Sandbox: policy.Sandbox("workspace-write")})
	if first.boundCwd == nil || countFDs() <= baseline {
		t.Fatalf("session did not hold a cwd descriptor: baseline=%d, current=%d", baseline, countFDs())
	}
	fd := int(first.boundCwd.File().Fd())
	var latest *Session
	for i := 0; i < 80; i++ {
		current = current.Add(25 * time.Hour)
		latest = manager.Create(Options{Cwd: cwd, Sandbox: policy.Sandbox("workspace-write")})
		if got := countFDs(); got > baseline+8 {
			t.Fatalf("file descriptors accumulated after %d evictions: baseline=%d, current=%d", i+1, baseline, got)
		}
	}
	latestFD := int(latest.boundCwd.File().Fd())
	latest.Close()
	var stat syscall.Stat_t
	if err := syscall.Fstat(latestFD, &stat); err != syscall.EBADF {
		t.Fatalf("closed session fd %d Fstat error = %v, want EBADF", latestFD, err)
	}
	manager.Close()
	if got := countFDs(); got > baseline+1 {
		t.Fatalf("file descriptors remain after Close: baseline=%d, current=%d", baseline, got)
	}
	if err := syscall.Fstat(fd, &stat); err != syscall.EBADF {
		t.Fatalf("evicted session fd %d Fstat error = %v, want EBADF", fd, err)
	}
}

func TestSessionMissingBoundRootFailsClosed(t *testing.T) {
	cwd := t.TempDir()
	missing := filepath.Join(t.TempDir(), "missing")
	manager := NewManager()
	t.Cleanup(manager.Close)
	s := manager.Create(Options{Cwd: cwd, Sandbox: policy.Sandbox("workspace-write"), Approval: policy.ApprovalPolicy("never"), WritableRoots: []string{missing}})
	r := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	result, failed := r.execToolCall(context.Background(), s, toolCall("missing-root", "shell", `{"command":"echo ok > f"}`))
	if !failed || !strings.Contains(result, "bind writable root") {
		t.Fatalf("missing root shell = (%q, failed=%v), want bind error", result, failed)
	}
	if _, err := os.Stat(filepath.Join(cwd, "f")); !os.IsNotExist(err) {
		t.Fatalf("shell ran with missing bound root: %v", err)
	}
}

func TestRunnerApprovedShellCallIsUnsandboxed(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		testutil.LandlockUnavailable(t, err)
	}
	outside, err := os.MkdirTemp(".", "approved-outside-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(outside) })
	outside, err = filepath.Abs(outside)
	if err != nil {
		t.Fatal(err)
	}
	manager := NewManager()
	t.Cleanup(manager.Close)
	s := manager.Create(Options{Cwd: t.TempDir(), Sandbox: policy.Sandbox("workspace-write"), Approval: policy.ApprovalPolicy("untrusted")})
	approver := &stubApprover{approved: true}
	r := &Runner{Emitter: &recEmitter{}, Approver: approver}
	path := filepath.Join(outside, "approved")
	result, failed := r.execToolCall(context.Background(), s, toolCall("approved", "shell", fmt.Sprintf(`{"command":%q}`, fmt.Sprintf("printf approved > %q", path))))
	if failed || !strings.Contains(result, "exit code: 0") {
		t.Fatalf("approved shell = (%q, failed=%v)", result, failed)
	}
	if got := approver.recordedRequests(); len(got) != 1 || got[0].Tool != "shell" {
		t.Fatalf("approvals = %+v, want one shell approval", got)
	}
	if data, err := os.ReadFile(path); err != nil || string(data) != "approved" {
		t.Fatalf("approved unsandboxed write = %q, %v", data, err)
	}
}
