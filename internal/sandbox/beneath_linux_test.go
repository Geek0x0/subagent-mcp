//go:build linux

package sandbox

import (
	"errors"
	"io/fs"
	"os"
	"path/filepath"
	"testing"
)

func newBeneath(t *testing.T) (root, outside string, b *Beneath) {
	t.Helper()
	base := t.TempDir()
	root, outside = filepath.Join(base, "root"), filepath.Join(base, "outside")
	for _, dir := range []string{root, outside} {
		if err := os.Mkdir(dir, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	dir, err := BindDirectory(root)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = dir.Close() })
	return root, outside, dir.FS().(*Beneath)
}

func TestBeneathOperations(t *testing.T) {
	root, _, b := newBeneath(t)
	p := func(rel string) string { return filepath.Join(b.dir.Path(), rel) }

	if err := b.WriteFile(p("a/b/c.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	if data, err := os.ReadFile(filepath.Join(root, "a/b/c.txt")); err != nil || string(data) != "hello" {
		t.Fatalf("file on disk = %q, %v", data, err)
	}
	if data, err := b.ReadFile(p("a/b/c.txt")); err != nil || string(data) != "hello" {
		t.Fatalf("ReadFile = %q, %v", data, err)
	}
	if info, err := b.Lstat(p("a/b/c.txt")); err != nil || info.Size() != 5 {
		t.Fatalf("Lstat = %v, %v", info, err)
	}
	if _, err := b.Stat(p("missing")); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("Stat(missing) error = %v, want not-exist", err)
	}
	if err := b.Symlink("c.txt", p("a/b/link")); err != nil {
		t.Fatalf("Symlink: %v", err)
	}
	if target, err := b.Readlink(p("a/b/link")); err != nil || target != "c.txt" {
		t.Fatalf("Readlink = %q, %v", target, err)
	}
	if data, err := b.ReadFile(p("a/b/link")); err != nil || string(data) != "hello" {
		t.Fatalf("read through an in-tree symlink = %q, %v", data, err)
	}
	if err := b.Chmod(p("a/b/c.txt"), 0o600); err != nil {
		t.Fatalf("Chmod: %v", err)
	}
	if info, _ := os.Stat(filepath.Join(root, "a/b/c.txt")); info.Mode().Perm() != 0o600 {
		t.Fatalf("mode = %v, want 0600", info.Mode().Perm())
	}
	if err := b.Rename(p("a/b/c.txt"), p("a/moved.txt")); err != nil {
		t.Fatalf("Rename: %v", err)
	}
	if err := b.Remove(p("a/moved.txt")); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	if err := b.Remove(p("a/moved.txt")); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("second Remove error = %v, want not-exist", err)
	}
}

func TestBeneathRefusesToLeaveTheDirectory(t *testing.T) {
	root, outside, b := newBeneath(t)
	p := func(rel string) string { return filepath.Join(b.dir.Path(), rel) }
	if err := os.WriteFile(filepath.Join(outside, "secret"), []byte("s"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(root, "abs")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("../outside", filepath.Join(root, "rel")); err != nil {
		t.Fatal(err)
	}

	for _, rel := range []string{"abs/new.txt", "rel/new.txt", "abs/secret", "rel/secret"} {
		if err := b.WriteFile(p(rel), []byte("x"), 0o644); err == nil {
			t.Errorf("WriteFile(%s) succeeded through a symlink that leaves the directory", rel)
		}
		if _, err := b.ReadFile(p(rel)); err == nil {
			t.Errorf("ReadFile(%s) succeeded through a symlink that leaves the directory", rel)
		}
	}
	if _, err := os.Stat(filepath.Join(outside, "new.txt")); err == nil {
		t.Error("a file appeared outside the directory")
	}
	if err := b.Remove(p("abs/secret")); err == nil {
		t.Error("Remove(abs/secret) removed a file outside the directory")
	}
	if _, err := os.Stat(filepath.Join(outside, "secret")); err != nil {
		t.Errorf("outside/secret: %v", err)
	}
	if err := b.WriteFile(filepath.Join(outside, "direct.txt"), []byte("x"), 0o644); err == nil {
		t.Error("WriteFile accepted a path outside the directory's own path")
	}
}
