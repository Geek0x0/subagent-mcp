//go:build linux

package sandbox

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"

	"github.com/landlock-lsm/go-landlock/landlock"
	llsyscall "github.com/landlock-lsm/go-landlock/landlock/syscall"
)

// Available reports whether the kernel supports Landlock.
func Available() error {
	if _, err := llsyscall.LandlockGetABIVersion(); err != nil {
		return fmt.Errorf("landlock unavailable: %w", err)
	}
	return nil
}

// run restricts the current process and execs argv; it only returns on failure.
func run(args []string) error {
	parsed, err := parseArgs(args)
	if err != nil {
		return err
	}
	if parsed.cwdFD >= 0 {
		if err := syscall.Fchdir(parsed.cwdFD); err != nil {
			return fmt.Errorf("sandbox helper: enter bound cwd: %w", err)
		}
	}
	// BestEffort silently becomes a no-op on kernels without Landlock, so require ABI >= 1 first.
	abi, err := llsyscall.LandlockGetABIVersion()
	if err != nil {
		return fmt.Errorf("landlock unavailable: %w", err)
	}
	roots := append([]string(nil), parsed.roots...)
	for _, fd := range parsed.rootFDs {
		roots = append(roots, filepath.Join("/proc/self/fd", fmt.Sprint(fd)))
	}
	writable := landlock.RWDirs(roots...)
	// "refer" permits rename/link between directories inside the roots. ABI v1 cannot grant it,
	// and requesting it there makes BestEffort drop the whole ruleset, so only ask on v2+.
	if abi >= 2 {
		writable = writable.WithRefer()
	}
	if err := landlock.V10.BestEffort().RestrictPaths(
		landlock.RODirs("/"),
		writable,
		landlock.RWFiles(deviceFiles...).IgnoreIfMissing(),
	); err != nil {
		return fmt.Errorf("landlock unavailable: %w", err)
	}
	path, err := exec.LookPath(parsed.argv[0])
	if err != nil {
		return err
	}
	for _, fd := range parsed.rootFDs {
		_ = syscall.Close(fd)
	}
	if parsed.cwdFD >= 0 {
		_ = syscall.Close(parsed.cwdFD)
	}
	return syscall.Exec(path, parsed.argv, os.Environ())
}
