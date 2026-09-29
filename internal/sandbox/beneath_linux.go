//go:build linux

package sandbox

import (
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/unix"

	"github.com/Geek0x0/subagent-mcp/internal/patch"
)

// Beneath is a file system rooted at a bound Directory. Every access is a single
// openat2/…at call relative to the held directory with RESOLVE_BENEATH, so the
// kernel refuses any path that would leave the directory, whatever a concurrent
// process does to intermediate components between a caller's check and the use.
// Symlinks that stay beneath the directory keep working.
type Beneath struct{ dir *Directory }

// FS returns the directory's Beneath file system. It is nil where the platform
// has no such enforcement (see beneath_other.go); callers then fall back to
// ordinary paths.
func (d *Directory) FS() patch.FS { return &Beneath{dir: d} }

func (b *Beneath) root() int { return int(b.dir.File().Fd()) }

// rel turns a path under the directory's Path() into a path relative to it.
func (b *Beneath) rel(op, name string) (string, error) {
	root := b.dir.Path()
	clean := filepath.Clean(name)
	if clean == root {
		return ".", nil
	}
	if rest, ok := strings.CutPrefix(clean, root+"/"); ok {
		return rest, nil
	}
	return "", &fs.PathError{Op: op, Path: name, Err: unix.EXDEV}
}

func (b *Beneath) open(op, name, rel string, flags int, mode uint32) (int, error) {
	fd, err := unix.Openat2(b.root(), rel, &unix.OpenHow{
		Flags:   uint64(flags | unix.O_CLOEXEC),
		Mode:    uint64(mode),
		Resolve: unix.RESOLVE_BENEATH,
	})
	if err != nil {
		return -1, &fs.PathError{Op: op, Path: name, Err: err}
	}
	return fd, nil
}

// parent opens the directory that holds rel and returns it with the last name.
func (b *Beneath) parent(op, name, rel string) (int, string, error) {
	dir, base := filepath.Split(rel)
	dir = filepath.Clean(dir)
	if rel == "." || base == "" {
		return -1, "", &fs.PathError{Op: op, Path: name, Err: unix.EINVAL}
	}
	if dir == "." {
		fd, err := unix.Dup(b.root())
		if err != nil {
			return -1, "", &fs.PathError{Op: op, Path: name, Err: err}
		}
		return fd, base, nil
	}
	fd, err := b.open(op, name, dir, unix.O_PATH|unix.O_DIRECTORY, 0)
	return fd, base, err
}

func (b *Beneath) statFlags(op, name string, flags int) (fs.FileInfo, error) {
	rel, err := b.rel(op, name)
	if err != nil {
		return nil, err
	}
	fd, err := b.open(op, name, rel, unix.O_PATH|flags, 0)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(fd), name)
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return nil, &fs.PathError{Op: op, Path: name, Err: err}
	}
	return info, nil
}

func (b *Beneath) Stat(name string) (fs.FileInfo, error) { return b.statFlags("stat", name, 0) }
func (b *Beneath) Lstat(name string) (fs.FileInfo, error) {
	return b.statFlags("lstat", name, unix.O_NOFOLLOW)
}

func (b *Beneath) ReadFile(name string) ([]byte, error) {
	rel, err := b.rel("open", name)
	if err != nil {
		return nil, err
	}
	fd, err := b.open("open", name, rel, unix.O_RDONLY, 0)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(fd), name)
	defer file.Close()
	data, err := io.ReadAll(file)
	if err != nil {
		return nil, &fs.PathError{Op: "read", Path: name, Err: err}
	}
	return data, nil
}

func (b *Beneath) Readlink(name string) (string, error) {
	rel, err := b.rel("readlink", name)
	if err != nil {
		return "", err
	}
	fd, err := b.open("readlink", name, rel, unix.O_PATH|unix.O_NOFOLLOW, 0)
	if err != nil {
		return "", err
	}
	defer unix.Close(fd)
	buf := make([]byte, 4096)
	n, err := unix.Readlinkat(fd, "", buf)
	if err != nil {
		return "", &fs.PathError{Op: "readlink", Path: name, Err: err}
	}
	return string(buf[:n]), nil
}

func (b *Beneath) MkdirAll(name string, perm fs.FileMode) error {
	rel, err := b.rel("mkdir", name)
	if err != nil {
		return err
	}
	return b.mkdirAllRel(name, rel, perm)
}

func (b *Beneath) mkdirAllRel(name, rel string, perm fs.FileMode) error {
	if rel == "." {
		return nil
	}
	prefix := ""
	for _, part := range strings.Split(rel, "/") {
		prefix = filepath.Join(prefix, part)
		fd, err := b.open("mkdir", name, prefix, unix.O_PATH|unix.O_DIRECTORY, 0)
		if err == nil {
			unix.Close(fd)
			continue
		}
		if !errors.Is(err, fs.ErrNotExist) {
			return err
		}
		parent, base, err := b.parent("mkdir", name, prefix)
		if err != nil {
			return err
		}
		err = unix.Mkdirat(parent, base, uint32(perm.Perm()))
		unix.Close(parent)
		if err != nil && err != unix.EEXIST {
			return &fs.PathError{Op: "mkdir", Path: name, Err: err}
		}
	}
	return nil
}

func (b *Beneath) WriteFile(name string, data []byte, perm fs.FileMode) error {
	rel, err := b.rel("open", name)
	if err != nil {
		return err
	}
	if err := b.mkdirAllRel(name, filepath.Dir(rel), 0o755); err != nil {
		return err
	}
	fd, err := b.open("open", name, rel, unix.O_WRONLY|unix.O_CREAT|unix.O_TRUNC, uint32(perm.Perm()))
	if err != nil {
		return err
	}
	file := os.NewFile(uintptr(fd), name)
	if _, err := file.Write(data); err != nil {
		file.Close()
		return &fs.PathError{Op: "write", Path: name, Err: err}
	}
	return file.Close()
}

func (b *Beneath) Remove(name string) error {
	rel, err := b.rel("remove", name)
	if err != nil {
		return err
	}
	parent, base, err := b.parent("remove", name, rel)
	if err != nil {
		return err
	}
	defer unix.Close(parent)
	err = unix.Unlinkat(parent, base, 0)
	if err == unix.EISDIR || err == unix.EPERM {
		if dirErr := unix.Unlinkat(parent, base, unix.AT_REMOVEDIR); dirErr == nil {
			return nil
		}
	}
	if err != nil {
		return &fs.PathError{Op: "remove", Path: name, Err: err}
	}
	return nil
}

func (b *Beneath) Rename(oldName, newName string) error {
	oldRel, err := b.rel("rename", oldName)
	if err != nil {
		return err
	}
	newRel, err := b.rel("rename", newName)
	if err != nil {
		return err
	}
	oldParent, oldBase, err := b.parent("rename", oldName, oldRel)
	if err != nil {
		return err
	}
	defer unix.Close(oldParent)
	newParent, newBase, err := b.parent("rename", newName, newRel)
	if err != nil {
		return err
	}
	defer unix.Close(newParent)
	if err := unix.Renameat(oldParent, oldBase, newParent, newBase); err != nil {
		return &os.LinkError{Op: "rename", Old: oldName, New: newName, Err: err}
	}
	return nil
}

func (b *Beneath) Symlink(target, name string) error {
	rel, err := b.rel("symlink", name)
	if err != nil {
		return err
	}
	parent, base, err := b.parent("symlink", name, rel)
	if err != nil {
		return err
	}
	defer unix.Close(parent)
	if err := unix.Symlinkat(target, parent, base); err != nil {
		return &os.LinkError{Op: "symlink", Old: target, New: name, Err: err}
	}
	return nil
}

func (b *Beneath) Chmod(name string, mode fs.FileMode) error {
	rel, err := b.rel("chmod", name)
	if err != nil {
		return err
	}
	parent, base, err := b.parent("chmod", name, rel)
	if err != nil {
		return err
	}
	defer unix.Close(parent)
	if err := unix.Fchmodat(parent, base, uint32(mode.Perm()), 0); err != nil {
		return &fs.PathError{Op: "chmod", Path: name, Err: err}
	}
	return nil
}
