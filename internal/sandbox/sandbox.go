// Package sandbox runs commands under a kernel-enforced write restriction by
// re-executing the subagent-mcp binary in a hidden helper mode.
package sandbox

import (
	"errors"
	"fmt"
	"os"
	"strconv"
	"syscall"
)

// HelperArg is the hidden argv[1] that turns the subagent-mcp binary into the sandbox helper.
const HelperArg = "__sandbox-exec"

// deviceFiles are always writable under the sandbox; the rest of /dev (including
// /dev/shm and block devices) stays read-only.
var deviceFiles = []string{"/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty"}

// Command returns the argv that runs argv through the helper at self with only
// writableRoots writable.
func Command(self string, writableRoots []string, argv ...string) []string {
	out := []string{self, HelperArg}
	for _, root := range writableRoots {
		out = append(out, "--rw", root)
	}
	out = append(out, "--")
	return append(out, argv...)
}

// CommandFD passes a held cwd and writable directory handles to the helper.
// Descriptor numbers refer to exec.Cmd.ExtraFiles in the child (starting at 3).
func CommandFD(self string, cwdFD int, writableFDs []int, argv ...string) []string {
	out := []string{self, HelperArg, "--cwd-fd", strconv.Itoa(cwdFD)}
	for _, fd := range writableFDs {
		out = append(out, "--rw-fd", strconv.Itoa(fd))
	}
	out = append(out, "--")
	return append(out, argv...)
}

// MaybeRunHelper runs the sandbox helper and exits when os.Args selects it;
// otherwise it returns immediately. Call it first in main and in TestMain.
func MaybeRunHelper() {
	if len(os.Args) < 2 || os.Args[1] != HelperArg {
		return
	}
	err := run(os.Args[2:])
	fmt.Fprintf(os.Stderr, "subagent-mcp: %v\n", err)
	os.Exit(126)
}

type helperArgs struct {
	roots   []string
	rootFDs []int
	cwdFD   int
	argv    []string
}

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

func parseArgs(args []string) (helperArgs, error) {
	parsed := helperArgs{cwdFD: -1}
	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--rw":
			if i+1 >= len(args) {
				return helperArgs{}, errors.New("sandbox helper: --rw requires a path")
			}
			parsed.roots = append(parsed.roots, args[i+1])
			i++
		case "--rw-fd", "--cwd-fd":
			flag := args[i]
			if i+1 >= len(args) {
				return helperArgs{}, fmt.Errorf("sandbox helper: %s requires a directory fd", flag)
			}
			fd, err := parseDirectoryFD(args[i+1])
			if err != nil {
				return helperArgs{}, err
			}
			if flag == "--rw-fd" {
				parsed.rootFDs = append(parsed.rootFDs, fd)
			} else {
				if parsed.cwdFD >= 0 {
					return helperArgs{}, errors.New("sandbox helper: duplicate --cwd-fd")
				}
				parsed.cwdFD = fd
			}
			i++
		case "--":
			if i+1 >= len(args) {
				return helperArgs{}, errors.New("sandbox helper: missing command after --")
			}
			parsed.argv = args[i+1:]
			return parsed, nil
		default:
			return helperArgs{}, fmt.Errorf("sandbox helper: unexpected argument %q", args[i])
		}
	}
	return helperArgs{}, errors.New("sandbox helper: missing -- separator")
}
