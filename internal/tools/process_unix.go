//go:build !windows

package tools

import (
	"os/exec"
	"syscall"
)

func runManagedCommand(cmd *exec.Cmd) error {
	// ponytail: Linux process-group signaling kills ordinary descendants, and WaitDelay bounds
	// inherited-pipe waits. A descendant that escapes the group can survive without being reported.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL) }
	err := cmd.Run()
	sweepProcessGroup(cmd)
	return err
}

// sweepProcessGroup kills whatever is left in the process group started for
// cmd. cmd.Cancel only fires while the direct child is alive, and WaitDelay
// only covers descendants that kept the inherited pipes, so a command that
// backgrounds a descendant with its own redirection — "( work ) >/dev/null 2>&1 &"
// — returns while the descendant is still running. Sweeping the group after
// Wait reaps that survivor instead of leaving it behind for the rest of the
// server's life. A command that backgrounds work on purpose, to keep a dev
// server up say, therefore does not outlive its call.
//
// The probe narrows the risk of signalling a reused process group but does not
// eliminate it. It skips the kill whenever the group has no members, which is
// the common case, and a group that still has one can only hold this call's
// descendants. Two windows remain in which the last member could exit and its
// pgid be reused before the kill: between Wait returning and the probe, and
// between the probe and the kill.
//
// ponytail: A PID namespace or a cgroup scope is what actually closes those
// windows; the probe only makes them far narrower than an unconditional kill.
func sweepProcessGroup(cmd *exec.Cmd) {
	if cmd.Process == nil {
		return
	}
	pgid := cmd.Process.Pid
	if err := syscall.Kill(-pgid, 0); err != nil {
		return
	}
	_ = syscall.Kill(-pgid, syscall.SIGKILL)
}
