//go:build linux

package sandbox

import (
	"fmt"
	"os"
	"path/filepath"

	"golang.org/x/sys/unix"
)

// Directory holds the directory inode selected when a session is created.
type Directory struct {
	path     string
	resolved string
	file     *os.File
}

func BindDirectory(path string) (*Directory, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return nil, err
	}
	fd, err := unix.Open(absolute, unix.O_PATH|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(fd), absolute)
	resolved, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		_ = file.Close()
		return nil, err
	}
	return &Directory{path: absolute, resolved: resolved, file: file}, nil
}

func (d *Directory) File() *os.File { return d.file }

func (d *Directory) ResolvedPath() string { return d.resolved }

// Path reaches the held inode even when its original name has been replaced.
func (d *Directory) Path() string { return fmt.Sprintf("/proc/self/fd/%d", d.file.Fd()) }

// Check requires the session's original path still to name its bound inode.
func (d *Directory) Check() error {
	bound, err := d.file.Stat()
	if err != nil {
		return fmt.Errorf("bound directory %q is unavailable: %w", d.path, err)
	}
	current, err := os.Stat(d.path)
	if err != nil {
		return fmt.Errorf("bound directory %q is no longer reachable: %w", d.path, err)
	}
	if !os.SameFile(bound, current) {
		return fmt.Errorf("bound directory %q no longer names the session's original directory", d.path)
	}
	return nil
}

func (d *Directory) Close() error { return d.file.Close() }
