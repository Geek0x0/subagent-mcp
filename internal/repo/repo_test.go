package repo

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func writeFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestRoot(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, ".git", "HEAD"), "ref: refs/heads/main\n")
	nested := filepath.Join(root, "a", "b")
	if err := os.MkdirAll(nested, 0o755); err != nil {
		t.Fatal(err)
	}

	if got, ok := Root(nested); !ok || got != root {
		t.Fatalf("Root(nested) = (%q, %v), want (%q, true)", got, ok, root)
	}
	// A temp dir is only a non-repo when no parent of the temp root holds a .git.
	if nonRepo := t.TempDir(); !testutil.GitAncestor(nonRepo) {
		if _, ok := Root(nonRepo); ok {
			t.Fatalf("Root(non-repo) ok = true, want false")
		}
	}
}

func TestRootAcceptsGitFile(t *testing.T) {
	root := t.TempDir()
	writeFile(t, filepath.Join(root, ".git"), "gitdir: /elsewhere\n")
	if got, ok := Root(root); !ok || got != root {
		t.Fatalf("Root() = (%q, %v), want (%q, true)", got, ok, root)
	}
}

func TestHead(t *testing.T) {
	t.Run("loose ref", func(t *testing.T) {
		root := t.TempDir()
		writeFile(t, filepath.Join(root, ".git", "HEAD"), "ref: refs/heads/main\n")
		writeFile(t, filepath.Join(root, ".git", "refs", "heads", "main"), "abc123\n")
		if branch, commit := Head(root); branch != "main" || commit != "abc123" {
			t.Fatalf("Head() = (%q, %q)", branch, commit)
		}
	})
	t.Run("packed ref", func(t *testing.T) {
		root := t.TempDir()
		writeFile(t, filepath.Join(root, ".git", "HEAD"), "ref: refs/heads/dev\n")
		writeFile(t, filepath.Join(root, ".git", "packed-refs"), "# pack-refs\ndef456 refs/heads/dev\n")
		if branch, commit := Head(root); branch != "dev" || commit != "def456" {
			t.Fatalf("Head() = (%q, %q)", branch, commit)
		}
	})
	t.Run("detached", func(t *testing.T) {
		root := t.TempDir()
		writeFile(t, filepath.Join(root, ".git", "HEAD"), "0123abcd\n")
		if branch, commit := Head(root); branch != "" || commit != "0123abcd" {
			t.Fatalf("Head() = (%q, %q)", branch, commit)
		}
	})
	t.Run("worktree git file with commondir", func(t *testing.T) {
		main := t.TempDir()
		worktreeGitDir := filepath.Join(main, ".git", "worktrees", "wt")
		writeFile(t, filepath.Join(worktreeGitDir, "HEAD"), "ref: refs/heads/feature\n")
		writeFile(t, filepath.Join(worktreeGitDir, "commondir"), "../..\n")
		writeFile(t, filepath.Join(main, ".git", "refs", "heads", "feature"), "fea7\n")
		wt := t.TempDir()
		writeFile(t, filepath.Join(wt, ".git"), "gitdir: "+worktreeGitDir+"\n")
		if branch, commit := Head(wt); branch != "feature" || commit != "fea7" {
			t.Fatalf("Head() = (%q, %q)", branch, commit)
		}
	})
	t.Run("missing HEAD", func(t *testing.T) {
		if branch, commit := Head(t.TempDir()); branch != "" || commit != "" {
			t.Fatalf("Head() = (%q, %q), want empty", branch, commit)
		}
	})
}
