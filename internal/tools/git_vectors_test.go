package tools

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/policy"
)

// gitVectorRepo builds a repository with one tracked, stat-dirty file so that
// status/diff refresh the index (which runs filters and the post-index-change
// hook), and returns the repo, the hook script and its marker file.
func gitVectorRepo(t *testing.T) (home, repo, hook, marker string) {
	t.Helper()
	home, repo = t.TempDir(), t.TempDir()
	markerRoot := t.TempDir()
	t.Setenv("TMPDIR", markerRoot)
	marker = filepath.Join(markerRoot, "marker")
	hook = filepath.Join(markerRoot, "hook.sh")
	writeGitHook(t, hook, marker)
	gitRun(t, home, repo, "init", "-q")
	gitRun(t, home, repo, "config", "user.email", "test@example.com")
	gitRun(t, home, repo, "config", "user.name", "Test")
	if err := os.WriteFile(filepath.Join(repo, "f.txt"), []byte("one\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	gitRun(t, home, repo, "add", "f.txt")
	gitRun(t, home, repo, "commit", "-qm", "initial")
	old := time.Date(2001, 1, 1, 0, 0, 0, 0, time.UTC)
	if err := os.Chtimes(filepath.Join(repo, "f.txt"), old, old); err != nil {
		t.Fatal(err)
	}
	return home, repo, hook, marker
}

func markerExists(t *testing.T, marker string) bool {
	t.Helper()
	_, err := os.Stat(marker)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		t.Fatal(err)
	}
	return err == nil
}

// runsProgram reports whether the command, run through the wrapper, executed the
// repository's program (the marker appeared), and whether plain git does.
func runsProgram(t *testing.T, home, repo, marker, command string) (wrapped, plain bool) {
	t.Helper()
	round := 0
	dirty := func() {
		// A new mtime each time, or the refreshed index would still match the file.
		round++
		old := time.Date(2001+round, 1, 1, 0, 0, 0, 0, time.UTC)
		if err := os.Chtimes(filepath.Join(repo, "f.txt"), old, old); err != nil {
			t.Fatal(err)
		}
		_ = os.Remove(marker)
	}
	// Plain git first: it refreshes (and rewrites) the index, so the wrapped run
	// must start from a freshly dirtied file again.
	dirty()
	cmd := exec.Command("sh", "-c", command)
	cmd.Dir = repo
	cmd.Env = gitTestEnv(home)
	_ = cmd.Run()
	plain = markerExists(t, marker)

	dirty()
	if _, _, err := RunShell(context.Background(), repo, command, 10*time.Second); err != nil {
		t.Fatalf("RunShell(%q): %v", command, err)
	}
	wrapped = markerExists(t, marker)
	_ = os.Remove(marker)
	return wrapped, plain
}

func assertNeutralised(t *testing.T, home, repo, marker, command string) {
	t.Helper()
	wrapped, plain := runsProgram(t, home, repo, marker, command)
	if !plain {
		t.Fatalf("positive control failed: plain %q did not run the repository program, so this test proves nothing", command)
	}
	if wrapped {
		t.Errorf("%q through the wrapper ran the repository program", command)
	}
}

func TestGitFilterNameContainingEqualsIsNeutralised(t *testing.T) {
	home, repo, hook, marker := gitVectorRepo(t)
	if err := os.WriteFile(filepath.Join(repo, ".gitattributes"), []byte("f.txt filter=x=y\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	gitRun(t, home, repo, "config", "filter.x=y.clean", hook)
	assertNeutralised(t, home, repo, marker, "git diff")
}

func TestGitIndexRefreshHooksDoNotRun(t *testing.T) {
	home, repo, hook, marker := gitVectorRepo(t)
	hooks := filepath.Join(repo, ".git", "hooks")
	if err := os.MkdirAll(hooks, 0o755); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(hook)
	if err := os.WriteFile(filepath.Join(hooks, "post-index-change"), data, 0o700); err != nil {
		t.Fatal(err)
	}
	for _, command := range []string{"git status", "git diff"} {
		assertNeutralised(t, home, repo, marker, command)
	}
}

func TestGitSignaturePlaceholdersDoNotRunGpg(t *testing.T) {
	home, repo, hook, marker := gitVectorRepo(t)
	tree := gitOutput(t, home, repo, "rev-parse", "HEAD^{tree}")
	commit := "tree " + tree + "\nauthor A <a@example.com> 1 +0000\ncommitter A <a@example.com> 1 +0000\n" +
		"gpgsig -----BEGIN PGP SIGNATURE-----\n \n fake\n -----END PGP SIGNATURE-----\n\nsigned\n"
	hash := gitOutputStdin(t, home, repo, commit, "hash-object", "-t", "commit", "-w", "--stdin", "--literally")
	gitRun(t, home, repo, "update-ref", "HEAD", hash)
	gitRun(t, home, repo, "config", "gpg.program", hook)
	command := "git log --pretty=format:%GS -1"
	assertNeutralised(t, home, repo, marker, command)
	// The allowlist refuses the placeholder as well.
	if decision, _ := policy.Evaluate(policy.Sandbox("read-only"), policy.ApprovalPolicy("never"),
		policy.Request{Tool: "shell", Command: command, Cwd: repo}); decision == policy.Allow {
		t.Errorf("policy auto-allowed %q", command)
	}
}

func TestGitBlameTextconvFlagIsNeutralised(t *testing.T) {
	home, repo, hook, marker := gitVectorRepo(t)
	if err := os.WriteFile(filepath.Join(repo, ".gitattributes"), []byte("f.txt diff=zz\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	gitRun(t, home, repo, "config", "diff.zz.textconv", hook)
	command := "git blame --textconv f.txt"
	assertNeutralised(t, home, repo, marker, command)
	if decision, _ := policy.Evaluate(policy.Sandbox("read-only"), policy.ApprovalPolicy("never"),
		policy.Request{Tool: "shell", Command: command, Cwd: repo}); decision == policy.Allow {
		t.Errorf("policy auto-allowed %q", command)
	}
}

// The user's own global git config (safe.directory, includes, ...) still applies.
func TestGitWrapperHonoursGlobalConfig(t *testing.T) {
	home, repo, _, _ := gitVectorRepo(t)
	global := filepath.Join(home, ".gitconfig")
	t.Setenv("HOME", home)
	t.Setenv("GIT_CONFIG_GLOBAL", global)
	if err := os.WriteFile(global, []byte("[core]\n\tabbrev = 9\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	out, code, err := RunShell(context.Background(), repo, "git rev-parse --short HEAD", 10*time.Second)
	if err != nil || code != 0 || len(strings.TrimSpace(out)) != 9 {
		t.Fatalf("git rev-parse --short HEAD = (%q, %d, %v), want a 9-character hash from ~/.gitconfig", out, code, err)
	}
}

func gitOutput(t *testing.T, home, repo string, args ...string) string {
	t.Helper()
	return gitOutputStdin(t, home, repo, "", args...)
}

func gitOutputStdin(t *testing.T, home, repo, stdin string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", args...)
	cmd.Dir = repo
	cmd.Env = gitTestEnv(home)
	cmd.Stdin = strings.NewReader(stdin)
	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("git %s: %v", strings.Join(args, " "), err)
	}
	return strings.TrimSpace(string(out))
}
