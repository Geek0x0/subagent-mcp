//go:build !windows

package sandbox

import (
	"fmt"
	"strconv"
	"syscall"
)

func parseDirectoryFD(value string) (int, error) {
	fd, err := strconv.Atoi(value)
	if err != nil || fd < 3 {
		return 0, fmt.Errorf("sandbox helper: invalid directory fd %q", value)
	}
	var stat syscall.Stat_t
	if err := syscall.Fstat(fd, &stat); err != nil {
		return 0, fmt.Errorf("sandbox helper: inspect directory fd %d: %w", fd, err)
	}
	if stat.Mode&syscall.S_IFMT != syscall.S_IFDIR {
		return 0, fmt.Errorf("sandbox helper: fd %d is not a directory", fd)
	}
	return fd, nil
}
