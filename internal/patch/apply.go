package patch

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

type Change struct {
	Kind       Kind
	Path       string
	MoveTo     string
	NewContent string
	Diff       string
}

// fileState is the planned state of one path while a patch is planned.
type fileState struct {
	exists  bool
	content string
}

// planner threads an in-memory overlay through Plan: sections apply in order, so a
// later section sees the edits (and deletions) of the earlier ones instead of the
// file on disk. Paths are keyed by their cleaned absolute spelling.
type planner struct {
	files map[string]fileState
}

// Plan computes every resulting file in memory; it writes nothing. Sections that
// touch the same path compose in order or fail with an error naming the path.
func Plan(cwd string, hunks []Hunk) ([]Change, error) {
	p := &planner{files: make(map[string]fileState)}
	changes := make([]Change, 0, len(hunks))
	for _, hunk := range hunks {
		path := resolve(cwd, hunk.Path)
		switch hunk.Kind {
		case Add:
			if state, planned := p.files[path]; planned {
				if state.exists {
					return nil, fmt.Errorf("add %s: file already exists", hunk.Path)
				}
			} else if err := checkTarget(path, "add "+hunk.Path, false); err != nil {
				return nil, err
			}
			p.files[path] = fileState{exists: true, content: hunk.Content}
			changes = append(changes, Change{Kind: Add, Path: path, NewContent: hunk.Content, Diff: hunk.Content})
		case Delete:
			if state, planned := p.files[path]; planned {
				if !state.exists {
					return nil, fmt.Errorf("delete %s: %w", hunk.Path, fs.ErrNotExist)
				}
			} else {
				info, err := os.Stat(path)
				if err != nil {
					return nil, fmt.Errorf("delete %s: %w", hunk.Path, err)
				}
				if info.IsDir() {
					return nil, fmt.Errorf("delete %s: is a directory", hunk.Path)
				}
			}
			p.files[path] = fileState{}
			changes = append(changes, Change{Kind: Delete, Path: path})
		case Update:
			original, err := p.read(path)
			if err != nil {
				return nil, fmt.Errorf("update %s: %w", hunk.Path, err)
			}
			content, diff, err := applyChunks(original, hunk.Chunks)
			if err != nil {
				return nil, fmt.Errorf("update %s: %w", hunk.Path, err)
			}
			change := Change{Kind: Update, Path: path, NewContent: content, Diff: diff}
			if hunk.MoveTo != "" {
				change.MoveTo = resolve(cwd, hunk.MoveTo)
				if change.MoveTo != path {
					if state, planned := p.files[change.MoveTo]; planned {
						// Overwriting a file that an earlier section of this patch
						// created or wrote would silently drop that section.
						if state.exists {
							return nil, fmt.Errorf("move %s: file already exists", hunk.MoveTo)
						}
					} else if err := checkTarget(change.MoveTo, "move "+hunk.MoveTo, true); err != nil {
						return nil, err
					}
					p.files[path] = fileState{}
					p.files[change.MoveTo] = fileState{exists: true, content: content}
				}
			}
			if change.MoveTo == "" || change.MoveTo == path {
				p.files[path] = fileState{exists: true, content: content}
			}
			changes = append(changes, change)
		}
	}
	return changes, nil
}

// read returns the planned content of path: an earlier section's result if there
// is one, else the file on disk.
func (p *planner) read(path string) (string, error) {
	if state, planned := p.files[path]; planned {
		if !state.exists {
			return "", fs.ErrNotExist
		}
		return state.content, nil
	}
	data, err := os.ReadFile(path)
	return string(data), err
}

// checkTarget rejects, before anything is written, a target that Commit could not
// write as a regular file: an existing path (unless allowExisting), a directory, or
// a path whose parent is not a directory.
func checkTarget(path, label string, allowExisting bool) error {
	info, err := os.Lstat(path)
	switch {
	case err == nil && !allowExisting:
		return fmt.Errorf("%s: file already exists", label)
	case err == nil && info.IsDir():
		return fmt.Errorf("%s: is a directory", label)
	case err != nil && !errors.Is(err, fs.ErrNotExist):
		return fmt.Errorf("%s: %w", label, err)
	}
	return nil
}

func resolve(cwd, path string) string {
	if filepath.IsAbs(path) {
		return filepath.Clean(path)
	}
	return filepath.Join(cwd, path)
}

type replacement struct {
	start  int
	oldLen int
	lines  []string
	raw    []string
}

func applyChunks(original string, chunks []Chunk) (string, string, error) {
	// ponytail: A file containing any CRLF is treated as CRLF throughout; mixed-ending files are normalized to CRLF.
	crlf := strings.Contains(original, "\r\n")
	if crlf {
		original = strings.ReplaceAll(original, "\r\n", "\n")
	}
	trailingNewline := original == "" || strings.HasSuffix(original, "\n")
	var lines []string
	if original != "" {
		lines = strings.Split(strings.TrimSuffix(original, "\n"), "\n")
	}

	var replacements []replacement
	position := 0
	for _, chunk := range chunks {
		if chunk.Header != "" {
			at := seek(lines, []string{chunk.Header}, position, false)
			if at < 0 {
				return "", "", fmt.Errorf("context header %q not found", chunk.Header)
			}
			position = at + 1
		}
		if len(chunk.Old) == 0 {
			at := len(lines)
			if chunk.Header != "" {
				at = position
			}
			replacements = append(replacements, replacement{start: at, lines: chunk.New, raw: chunk.Raw})
			position = at
			continue
		}
		at := seek(lines, chunk.Old, position, chunk.EOF)
		if at < 0 {
			return "", "", fmt.Errorf("could not find chunk starting with %q", chunk.Old[0])
		}
		replacements = append(replacements, replacement{start: at, oldLen: len(chunk.Old), lines: chunk.New, raw: chunk.Raw})
		position = at + len(chunk.Old)
	}

	var out []string
	var diff strings.Builder
	previous, offset := 0, 0
	for _, r := range replacements {
		out = append(out, lines[previous:r.start]...)
		out = append(out, r.lines...)
		fmt.Fprintf(&diff, "@@ -%d,%d +%d,%d @@\n", r.start+1, r.oldLen, r.start+1+offset, len(r.lines))
		for _, line := range r.raw {
			diff.WriteString(line)
			diff.WriteByte('\n')
		}
		offset += len(r.lines) - r.oldLen
		previous = r.start + r.oldLen
	}
	out = append(out, lines[previous:]...)

	result := strings.Join(out, "\n")
	if trailingNewline && len(out) > 0 {
		result += "\n"
	}
	if crlf {
		result = strings.ReplaceAll(result, "\n", "\r\n")
	}
	return result, diff.String(), nil
}

var normalizers = []func(string) string{
	func(s string) string { return s },
	func(s string) string { return strings.TrimRight(s, " \t") },
	strings.TrimSpace,
}

// seek returns the first index at or after start where pattern matches, trying
// exact, trailing-whitespace-insensitive, then surrounding-whitespace-insensitive
// comparison; eof anchors the match at the end of lines.
func seek(lines, pattern []string, start int, eof bool) int {
	for _, normalize := range normalizers {
		if eof {
			at := len(lines) - len(pattern)
			if at >= start && matchAt(lines, pattern, at, normalize) {
				return at
			}
			continue
		}
		for at := start; at+len(pattern) <= len(lines); at++ {
			if matchAt(lines, pattern, at, normalize) {
				return at
			}
		}
	}
	return -1
}

func matchAt(lines, pattern []string, at int, normalize func(string) string) bool {
	for i, want := range pattern {
		if normalize(lines[at+i]) != normalize(want) {
			return false
		}
	}
	return true
}

// Commit writes planned changes in order. If any step fails it undoes the steps
// already taken, so every touched file is left as it was before the call; a failed
// undo is reported in the returned error.
// ponytail: Moved files are recreated with mode 0644; carry the source mode through Change if exec bits matter.
func Commit(changes []Change) error {
	tx := &commitTx{saved: make(map[string]snapshot)}
	for _, change := range changes {
		if err := tx.apply(change); err != nil {
			return tx.rollback(err)
		}
	}
	tx.finish()
	return nil
}

func (t *commitTx) apply(change Change) error {
	switch change.Kind {
	case Add:
		return t.write(change.Path, change.NewContent)
	case Delete:
		return t.remove(change.Path)
	case Update:
		target := change.Path
		if change.MoveTo != "" {
			target = change.MoveTo
		}
		if err := t.write(target, change.NewContent); err != nil {
			return err
		}
		if change.MoveTo != "" && change.MoveTo != change.Path {
			return t.remove(change.Path)
		}
	}
	return nil
}

// snapshot is what a path looked like before Commit first touched it.
type snapshot struct {
	trashed string // where remove set the original aside; restore renames it back
	exists  bool
	link    string // symlink target, when the path was a symlink
	data    []byte // file content (through the link for a symlink); nil if unreadable
	mode    os.FileMode
}

// commitTx records the original state of every path Commit touches, and the
// directories it creates, so a failure can put everything back.
type commitTx struct {
	saved  map[string]snapshot
	order  []string
	dirs   []string // created directories, deepest first
	trash  []string // files set aside by remove, deleted once Commit succeeds
	serial int
}

func (t *commitTx) save(path string) error {
	if _, done := t.saved[path]; done {
		return nil
	}
	info, err := os.Lstat(path)
	snap := snapshot{}
	switch {
	case errors.Is(err, fs.ErrNotExist):
	case err != nil:
		return err
	case info.IsDir():
		return fmt.Errorf("%s: is a directory", path)
	default:
		snap.exists, snap.mode = true, info.Mode()
		if info.Mode()&os.ModeSymlink != 0 {
			if snap.link, err = os.Readlink(path); err != nil {
				return err
			}
		}
		snap.data, _ = os.ReadFile(path)
		if snap.link == "" && snap.data == nil {
			// A regular file whose content cannot be captured cannot be restored.
			return fmt.Errorf("%s: cannot read the file to back it up", path)
		}
	}
	t.saved[path] = snap
	t.order = append(t.order, path)
	return nil
}

func (t *commitTx) write(path, content string) error {
	if err := t.save(path); err != nil {
		return err
	}
	if err := t.mkdirAll(filepath.Dir(path)); err != nil {
		return err
	}
	return os.WriteFile(path, []byte(content), 0o644)
}

// remove deletes a file by setting it aside in its own directory, so it can be put
// back exactly (any mode, any size, even unreadable) if a later step fails. The
// set-aside copies are deleted once every step has succeeded.
func (t *commitTx) remove(path string) error {
	if _, done := t.saved[path]; done {
		// Already written by this Commit: its original is in the snapshot.
		return os.Remove(path)
	}
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if info.IsDir() {
		return fmt.Errorf("%s: is a directory", path)
	}
	t.serial++
	trash := filepath.Join(filepath.Dir(path), fmt.Sprintf(".%s.subagent-mcp-trash-%d-%d", filepath.Base(path), os.Getpid(), t.serial))
	if err := os.Rename(path, trash); err != nil {
		return err
	}
	t.saved[path] = snapshot{exists: true, trashed: trash}
	t.order = append(t.order, path)
	t.trash = append(t.trash, trash)
	return nil
}

// finish deletes the set-aside originals after a successful Commit.
func (t *commitTx) finish() {
	for _, trash := range t.trash {
		_ = os.Remove(trash)
	}
}

func (t *commitTx) mkdirAll(dir string) error {
	var missing []string
	for d := dir; ; d = filepath.Dir(d) {
		if _, err := os.Lstat(d); !errors.Is(err, fs.ErrNotExist) {
			break
		}
		missing = append(missing, d)
		if filepath.Dir(d) == d {
			break
		}
	}
	err := os.MkdirAll(dir, 0o755)
	t.dirs = append(t.dirs, missing...)
	return err
}

// rollback restores every saved path in reverse order and removes the directories
// Commit created, then returns cause, extended with any restore failure.
func (t *commitTx) rollback(cause error) error {
	var failures []error
	for i := len(t.order) - 1; i >= 0; i-- {
		path := t.order[i]
		if err := t.saved[path].restore(path); err != nil {
			failures = append(failures, err)
		}
	}
	for _, dir := range t.dirs {
		// Best effort: a directory that is no longer empty is not ours to remove.
		_ = os.Remove(dir)
	}
	if len(failures) > 0 {
		return fmt.Errorf("%w (rollback failed, files may be left modified: %w)", cause, errors.Join(failures...))
	}
	return cause
}

func (s snapshot) restore(path string) error {
	switch {
	case s.trashed != "":
		// Whatever a later step put at the path goes; the original comes back.
		if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return err
		}
		return os.Rename(s.trashed, path)
	case !s.exists:
		if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return err
		}
		return nil
	case s.link != "":
		if info, err := os.Lstat(path); err != nil || info.Mode()&os.ModeSymlink == 0 || readlinkOrEmpty(path) != s.link {
			if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
				return err
			}
			if err := os.Symlink(s.link, path); err != nil {
				return err
			}
		}
		if s.data == nil {
			return nil
		}
		return os.WriteFile(path, s.data, 0o644)
	default:
		if err := os.WriteFile(path, s.data, s.mode.Perm()); err != nil {
			return err
		}
		return os.Chmod(path, s.mode.Perm())
	}
}

func readlinkOrEmpty(path string) string {
	target, _ := os.Readlink(path)
	return target
}

// Summary renders the Codex-style success message.
func Summary(changes []Change) string {
	var out strings.Builder
	out.WriteString("Success. Updated the following files:")
	for _, change := range changes {
		letter, path := "M", change.Path
		switch change.Kind {
		case Add:
			letter = "A"
		case Delete:
			letter = "D"
		}
		if change.MoveTo != "" {
			path = change.MoveTo
		}
		fmt.Fprintf(&out, "\n%s %s", letter, path)
	}
	return out.String()
}
