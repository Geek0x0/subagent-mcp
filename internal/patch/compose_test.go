package patch

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func patchText(lines ...string) string {
	return strings.Join(append(append([]string{"*** Begin Patch"}, lines...), "*** End Patch"), "\n")
}

func TestSameFileUpdatesCompose(t *testing.T) {
	cwd := t.TempDir()
	write(t, filepath.Join(cwd, "a.txt"), "a\nb\nc\n")

	if _, err := applyText(t, cwd, patchText(
		"*** Update File: a.txt", "@@", "-a", "+A",
		"*** Update File: a.txt", "@@", "-c", "+C",
	)); err != nil {
		t.Fatalf("apply error = %v", err)
	}
	if got := read(t, filepath.Join(cwd, "a.txt")); got != "A\nb\nC\n" {
		t.Errorf("a.txt = %q, want both edits (the first section was silently dropped)", got)
	}
}

func TestSecondSectionSeesTheFirstSectionsEdit(t *testing.T) {
	cwd := t.TempDir()
	write(t, filepath.Join(cwd, "a.txt"), "a\nb\n")

	if _, err := applyText(t, cwd, patchText(
		"*** Update File: a.txt", "@@", "-a", "+A",
		"*** Update File: a.txt", "@@", "-A", "+AA",
	)); err != nil {
		t.Fatalf("apply error = %v", err)
	}
	if got := read(t, filepath.Join(cwd, "a.txt")); got != "AA\nb\n" {
		t.Errorf("a.txt = %q, want the second section applied on top of the first", got)
	}
}

func TestSamePathSpellingsAreOneFile(t *testing.T) {
	cwd := t.TempDir()
	write(t, filepath.Join(cwd, "a.txt"), "a\nb\nc\n")

	if _, err := applyText(t, cwd, patchText(
		"*** Update File: a.txt", "@@", "-a", "+A",
		"*** Update File: ./a.txt", "@@", "-b", "+B",
		"*** Update File: "+filepath.Join(cwd, "a.txt"), "@@", "-c", "+C",
	)); err != nil {
		t.Fatalf("apply error = %v", err)
	}
	if got := read(t, filepath.Join(cwd, "a.txt")); got != "A\nB\nC\n" {
		t.Errorf("a.txt = %q, want all three spellings to edit the same file", got)
	}
}

func TestSamePathCombinations(t *testing.T) {
	tests := []struct {
		name    string
		files   map[string]string
		patch   string
		want    map[string]string // path -> content; "" means the file must not exist
		wantErr string            // substring of the error, which must name the path
	}{
		{
			name:  "add then update",
			patch: patchText("*** Add File: n.txt", "+one", "*** Update File: n.txt", "@@", "-one", "+ONE"),
			want:  map[string]string{"n.txt": "ONE\n"},
		},
		{
			name:  "update then update with move",
			files: map[string]string{"a.txt": "a\nb\nc\n"},
			patch: patchText(
				"*** Update File: a.txt", "@@", "-a", "+A",
				"*** Update File: a.txt", "*** Move to: b.txt", "@@", "-c", "+C"),
			want: map[string]string{"a.txt": "", "b.txt": "A\nb\nC\n"},
		},
		{
			name:  "delete then add",
			files: map[string]string{"x.txt": "old\n"},
			patch: patchText("*** Delete File: x.txt", "*** Add File: x.txt", "+new"),
			want:  map[string]string{"x.txt": "new\n"},
		},
		{
			name:    "add twice",
			patch:   patchText("*** Add File: n.txt", "+one", "*** Add File: n.txt", "+two"),
			wantErr: "n.txt",
		},
		{
			name:    "update after delete",
			files:   map[string]string{"x.txt": "old\n"},
			patch:   patchText("*** Delete File: x.txt", "*** Update File: x.txt", "@@", "-old", "+new"),
			wantErr: "x.txt",
		},
		{
			name:    "delete twice",
			files:   map[string]string{"x.txt": "old\n"},
			patch:   patchText("*** Delete File: x.txt", "*** Delete File: x.txt"),
			wantErr: "x.txt",
		},
		{
			name:    "update the source again after a move",
			files:   map[string]string{"a.txt": "a\nb\n"},
			patch:   patchText("*** Update File: a.txt", "*** Move to: b.txt", "@@", "-a", "+A", "*** Update File: a.txt", "@@", "-b", "+B"),
			wantErr: "a.txt",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cwd := t.TempDir()
			for path, content := range tt.files {
				write(t, filepath.Join(cwd, path), content)
			}
			_, err := applyText(t, cwd, tt.patch)
			if tt.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), tt.wantErr) {
					t.Fatalf("error = %v, want one naming %q", err, tt.wantErr)
				}
				for path, content := range tt.files {
					if got := read(t, filepath.Join(cwd, path)); got != content {
						t.Errorf("%s = %q after a rejected patch, want it untouched (%q)", path, got, content)
					}
				}
				return
			}
			if err != nil {
				t.Fatalf("apply error = %v", err)
			}
			for path, content := range tt.want {
				data, err := os.ReadFile(filepath.Join(cwd, path))
				switch {
				case content == "" && err == nil:
					t.Errorf("%s exists, want it gone", path)
				case content != "" && (err != nil || string(data) != content):
					t.Errorf("%s = %q (err %v), want %q", path, data, err, content)
				}
			}
		})
	}
}

func planOnly(t *testing.T, cwd, text string) []Change {
	t.Helper()
	hunks, err := Parse(text)
	if err != nil {
		t.Fatal(err)
	}
	changes, err := Plan(cwd, hunks)
	if err != nil {
		t.Fatal(err)
	}
	return changes
}

func TestCommitFailureLeavesEveryFileUntouched(t *testing.T) {
	tests := []struct {
		name    string
		patch   string
		disturb func(t *testing.T, cwd string)
	}{
		{
			name: "file deleted after planning",
			patch: patchText(
				"*** Update File: a.txt", "@@", "-one", "+ONE",
				"*** Add File: deep/er/n.txt", "+new",
				"*** Delete File: b.txt"),
			disturb: func(t *testing.T, cwd string) {
				if err := os.Remove(filepath.Join(cwd, "b.txt")); err != nil {
					t.Fatal(err)
				}
			},
		},
		{
			name: "target became a directory",
			patch: patchText(
				"*** Update File: a.txt", "@@", "-one", "+ONE",
				"*** Add File: deep/er/n.txt", "+new",
				"*** Update File: b.txt", "*** Move to: m.txt", "@@", "-bee", "+BEE"),
			disturb: func(t *testing.T, cwd string) {
				if err := os.Mkdir(filepath.Join(cwd, "m.txt"), 0o755); err != nil {
					t.Fatal(err)
				}
			},
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cwd := t.TempDir()
			write(t, filepath.Join(cwd, "a.txt"), "one\ntwo\n")
			write(t, filepath.Join(cwd, "b.txt"), "bee\n")
			if err := os.Chmod(filepath.Join(cwd, "a.txt"), 0o600); err != nil {
				t.Fatal(err)
			}
			changes := planOnly(t, cwd, tt.patch)
			tt.disturb(t, cwd)

			if err := Commit(changes); err == nil {
				t.Fatal("Commit() error = nil, want the disturbed step to fail")
			}

			if got := read(t, filepath.Join(cwd, "a.txt")); got != "one\ntwo\n" {
				t.Errorf("a.txt = %q after failed Commit, want it restored", got)
			}
			if info, err := os.Stat(filepath.Join(cwd, "a.txt")); err != nil || info.Mode().Perm() != 0o600 {
				t.Errorf("a.txt mode = %v (err %v), want 0600 preserved", info, err)
			}
			if _, err := os.Stat(filepath.Join(cwd, "deep")); !os.IsNotExist(err) {
				t.Errorf("directory deep/ created by the failed Commit still exists (err %v)", err)
			}
			if tt.name == "target became a directory" {
				if got := read(t, filepath.Join(cwd, "b.txt")); got != "bee\n" {
					t.Errorf("b.txt = %q after failed Commit, want it restored", got)
				}
			}
		})
	}
}

func TestRollbackFailureIsReported(t *testing.T) {
	dir := t.TempDir()
	blocker := filepath.Join(dir, "blocker")
	write(t, blocker, "a file, not a directory\n")

	tx := &commitTx{fs: OS, saved: map[string]snapshot{}}
	// The original of this path cannot be written back: its parent is a regular file.
	path := filepath.Join(blocker, "orig.txt")
	tx.saved[path] = snapshot{exists: true, data: []byte("orig\n"), mode: 0o644}
	tx.order = []string{path}

	cause := errors.New("write failed")
	err := tx.rollback(cause)
	if !errors.Is(err, cause) {
		t.Errorf("rollback() error = %v, want it to wrap the original failure", err)
	}
	if err == nil || !strings.Contains(err.Error(), "rollback failed") || !strings.Contains(err.Error(), "orig.txt") {
		t.Errorf("rollback() error = %v, want the failed restore of orig.txt reported", err)
	}
}

func TestHeaderPositionSelectsTheSecondBlock(t *testing.T) {
	cwd := t.TempDir()
	write(t, filepath.Join(cwd, "f.txt"), "dup\nbody\ndup\nbody\n")

	// The header line matches the first "dup"; the chunk that follows must be
	// searched after it, so the SECOND identical block is the one edited.
	if _, err := applyText(t, cwd, patchText(
		"*** Update File: f.txt", "@@ dup", " dup", "-body", "+BODY",
	)); err != nil {
		t.Fatalf("apply error = %v", err)
	}
	if got := read(t, filepath.Join(cwd, "f.txt")); got != "dup\nbody\ndup\nBODY\n" {
		t.Errorf("f.txt = %q, want only the second block edited", got)
	}
}

func TestMoveOntoAPlannedPathIsRejected(t *testing.T) {
	tests := map[string]string{
		"add then move onto it": patchText(
			"*** Add File: b.txt", "+added",
			"*** Update File: a.txt", "*** Move to: b.txt", "@@", "-a", "+A"),
		"two moves to one target": patchText(
			"*** Update File: a.txt", "*** Move to: b.txt", "@@", "-a", "+A",
			"*** Update File: c.txt", "*** Move to: b.txt", "@@", "-c", "+C"),
	}
	for name, text := range tests {
		t.Run(name, func(t *testing.T) {
			cwd := t.TempDir()
			write(t, filepath.Join(cwd, "a.txt"), "a\n")
			write(t, filepath.Join(cwd, "c.txt"), "c\n")
			_, err := applyText(t, cwd, text)
			if err == nil || !strings.Contains(err.Error(), "b.txt") {
				t.Fatalf("error = %v, want a rejection naming b.txt", err)
			}
			if got := read(t, filepath.Join(cwd, "a.txt")); got != "a\n" {
				t.Errorf("a.txt = %q, want it untouched", got)
			}
			if _, err := os.Stat(filepath.Join(cwd, "b.txt")); !os.IsNotExist(err) {
				t.Errorf("b.txt exists after a rejected patch")
			}
		})
	}
}

// Deleting works on files the process cannot read, leaves nothing behind on
// success, and a later failure puts the file back with its mode.
func TestDeleteUnreadableFileAndItsRollback(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("root reads mode-000 files")
	}
	cwd := t.TempDir()
	locked := filepath.Join(cwd, "locked.txt")
	write(t, locked, "secret\n")
	write(t, filepath.Join(cwd, "keep.txt"), "keep\n")
	if err := os.Chmod(locked, 0); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(locked, 0o600) })

	changes := planOnly(t, cwd, patchText("*** Delete File: locked.txt", "*** Delete File: keep.txt"))
	if err := os.Remove(filepath.Join(cwd, "keep.txt")); err != nil { // makes the second step fail
		t.Fatal(err)
	}
	if err := Commit(changes); err == nil {
		t.Fatal("Commit() error = nil, want the second delete to fail")
	}
	info, err := os.Stat(locked)
	if err != nil || info.Mode().Perm() != 0 {
		t.Fatalf("locked.txt after failed Commit = (%v, %v), want it back with mode 000", info, err)
	}

	changes = planOnly(t, cwd, patchText("*** Delete File: locked.txt"))
	if err := Commit(changes); err != nil {
		t.Fatalf("Commit() error = %v, want the unreadable file deleted", err)
	}
	entries, _ := os.ReadDir(cwd)
	if len(entries) != 0 {
		t.Errorf("directory holds %v after the delete, want it empty (no set-aside copy left)", entries)
	}
}
