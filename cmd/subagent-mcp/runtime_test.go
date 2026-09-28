package main

import (
	"context"
	"errors"
	"io"
	"os"
	"os/exec"
	"regexp"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/tools"
)

const procEnvironmentChild = "SUBAGENT_MCP_PROC_ENV_CHILD"

func TestConfigureRuntimeWarnsWhenProcProtectionFails(t *testing.T) {
	originalDisabler := disableProcessDumpingFn
	disableProcessDumpingFn = func() error { return errors.New("prctl: operation not permitted") }
	t.Cleanup(func() { disableProcessDumpingFn = originalDisabler })

	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatalf("os.Pipe() error = %v", err)
	}
	originalStderr := os.Stderr
	os.Stderr = writer
	configureRuntime(&config.Config{})
	if err := writer.Close(); err != nil {
		t.Fatalf("close warning pipe: %v", err)
	}
	os.Stderr = originalStderr
	defer reader.Close()

	warning, err := io.ReadAll(reader)
	if err != nil {
		t.Fatalf("read warning: %v", err)
	}
	output := string(warning)
	if !strings.Contains(output, "/proc environment protection could not be enabled") ||
		!strings.Contains(output, "prctl: operation not permitted") {
		t.Fatalf("warning = %q, want protection warning and disabler error", output)
	}
}

func TestServerEnvironmentIsNotReadableThroughProc(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("PR_SET_DUMPABLE is a Linux startup hardening measure")
	}

	const markerName = "SUBAGENT_TEST_PROC_ENV_MARKER"
	const markerValue = "proc-environment-secret-7c1e"
	if os.Getenv(procEnvironmentChild) == "1" {
		if got := os.Getenv(markerName); got != markerValue {
			t.Fatalf("re-exec environment marker = %q, want %q", got, markerValue)
		}

		configureRuntime(&config.Config{Providers: map[string]config.Provider{
			"test": {EnvKey: markerName},
		}})
		out, _, err := tools.RunShell(context.Background(), t.TempDir(), "cat /proc/$PPID/environ", time.Second)
		if err != nil {
			t.Fatalf("RunShell() error = %v; output = %q", err, out)
		}
		if strings.Contains(out, markerValue) {
			t.Fatal("shell child read the server environment marker")
		}
		return
	}

	cmd := exec.Command(os.Args[0], "-test.run=^"+regexp.QuoteMeta(t.Name())+"$")
	for _, entry := range os.Environ() {
		if strings.HasPrefix(entry, procEnvironmentChild+"=") || strings.HasPrefix(entry, markerName+"=") {
			continue
		}
		cmd.Env = append(cmd.Env, entry)
	}
	cmd.Env = append(cmd.Env, procEnvironmentChild+"=1", markerName+"="+markerValue)
	output, err := cmd.CombinedOutput()
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			t.Fatalf("re-exec failed with exit code %d; output:\n%s", exitErr.ExitCode(), output)
		}
		t.Fatalf("re-exec failed: %v; output:\n%s", err, output)
	}
}
