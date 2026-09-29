//go:build !linux

package sandbox

import (
	"fmt"
	"os"
	"path/filepath"
)

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
	file, err := os.Open(absolute)
	if err != nil {
		return nil, err
	}
	info, err := file.Stat()
	if err != nil || !info.IsDir() {
		_ = file.Close()
		return nil, fmt.Errorf("bound directory %q is not a directory", absolute)
	}
	resolved, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		_ = file.Close()
		return nil, err
	}
	return &Directory{path: absolute, resolved: resolved, file: file}, nil
}

func (d *Directory) File() *os.File       { return d.file }
func (d *Directory) ResolvedPath() string { return d.resolved }
func (d *Directory) Path() string         { return d.path }

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
