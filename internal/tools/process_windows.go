package tools

import (
	"fmt"
	"os/exec"
	"sync"
	"unsafe"

	"golang.org/x/sys/windows"
)

func runManagedCommand(cmd *exec.Cmd) error {
	job, err := windows.CreateJobObject(nil, nil)
	if err != nil {
		return fmt.Errorf("create process job: %w", err)
	}
	defer windows.CloseHandle(job)

	limits := windows.JOBOBJECT_EXTENDED_LIMIT_INFORMATION{}
	limits.BasicLimitInformation.LimitFlags = windows.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
	if _, err := windows.SetInformationJobObject(job, windows.JobObjectExtendedLimitInformation,
		uintptr(unsafe.Pointer(&limits)), uint32(unsafe.Sizeof(limits))); err != nil {
		return fmt.Errorf("configure process job: %w", err)
	}

	// Serialize cancellation with assignment, so cancellation cannot terminate
	// an empty job and then allow the started process to join it afterwards.
	var mu sync.Mutex
	cmd.Cancel = func() error {
		mu.Lock()
		defer mu.Unlock()
		return windows.TerminateJobObject(job, 1)
	}
	mu.Lock()
	if err := cmd.Start(); err != nil {
		mu.Unlock()
		return err
	}
	// ponytail: The child can spawn descendants before assignment to the job;
	// those descendants are not covered by the job and can outlive this call.
	process, err := windows.OpenProcess(windows.PROCESS_SET_QUOTA|windows.PROCESS_TERMINATE, false, uint32(cmd.Process.Pid))
	if err == nil {
		err = windows.AssignProcessToJobObject(job, process)
		windows.CloseHandle(process)
	}
	if err != nil {
		_ = cmd.Process.Kill()
		mu.Unlock()
		_ = cmd.Wait()
		return fmt.Errorf("assign process to job: %w", err)
	}
	mu.Unlock()
	return cmd.Wait()
}
