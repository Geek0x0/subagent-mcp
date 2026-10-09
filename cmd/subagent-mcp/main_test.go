package main

import (
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

const (
	mainChild  = "SUBAGENT_MCP_MAIN_CHILD"
	testEnvKey = "SUBAGENT_TEST_API_KEY"

	checkMainEnvKey   = "SUBAGENT_CHECK_TEST_API_KEY"
	checkMainPathEnv  = "SUBAGENT_CHECK_TEST_PATH"
	checkMainValueKey = "sk-check-child-secret"
	checkCodexEnvKey  = "SUBAGENT_CHECK_CODEX_NATIVE_KEY"
	checkCodexEnvFile = "SUBAGENT_FAKE_CODEX_ENV_FILE"

	validTestConfig = `
[providers.test]
api = "chat-completions"
env_key = "SUBAGENT_TEST_API_KEY"
default_model = "test-model"
models = [{ id = "test-model" }]
`
)

func TestMainMissingConfigFile(t *testing.T) {
	if runMainChild(t) {
		return
	}

	missing := filepath.Join(t.TempDir(), "missing.toml")
	output := runMainExpectingFailure(t, "SUBAGENT_MCP_CONFIG="+missing)
	for _, want := range []string{missing, "config.example.toml"} {
		if !strings.Contains(output, want) {
			t.Fatalf("main() failure output = %q, want it to contain %q", output, want)
		}
	}
}

func TestMainStartsWithoutAnyProviderKeySet(t *testing.T) {
	if runMainChild(t) {
		return
	}

	path := writeConfigFile(t, validTestConfig)
	cmd := newMainChildCommand(t)
	cmd.Env = append(cmd.Env, "SUBAGENT_MCP_CONFIG="+path)
	output, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("main() with no provider key set failed to start: %v; output:\n%s", err, output)
	}
}

func TestMainVersion(t *testing.T) {
	if os.Getenv(mainChild) == t.Name() {
		os.Args = []string{os.Args[0], "--version"}
		main()
		os.Exit(0)
	}

	home := t.TempDir()
	cmd := exec.Command(os.Args[0], "-test.run=^"+regexp.QuoteMeta(t.Name())+"$")
	for _, entry := range os.Environ() {
		if !hasEnvName(entry, "HOME") &&
			!hasEnvName(entry, mainChild) &&
			!hasEnvName(entry, "SUBAGENT_MCP_CONFIG") &&
			!hasEnvName(entry, "SUBAGENT_MCP_TOOL_NAME") &&
			!hasEnvName(entry, testEnvKey) {
			cmd.Env = append(cmd.Env, entry)
		}
	}
	cmd.Env = append(cmd.Env, "HOME="+home, mainChild+"="+t.Name())

	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		t.Fatalf("main() --version error = %v; stderr = %q", err, stderr.String())
	}
	if got, want := stdout.String(), "subagent-mcp "+version+"\n"; got != want {
		t.Fatalf("main() --version stdout = %q, want %q", got, want)
	}
}

func TestMainCheckConfigOK(t *testing.T) {
	if os.Getenv(mainChild) == t.Name() {
		os.Args = []string{os.Args[0], "--check-config"}
		main()
		os.Exit(0)
	}

	fake := testutil.NewFakeChat(t, nil)
	fake.SetModels([]string{"test-model"})
	contents := fmt.Sprintf(`
[providers.test]
api = "chat-completions"
base_url = %q
env_key = %q
default_model = "test-model"
models = [{ id = "test-model" }]
`, fake.URL, checkMainEnvKey)
	path := writeConfigFile(t, contents)

	cmd := newMainChildCommand(t)
	cmd.Env = append(cmd.Env, "SUBAGENT_MCP_CONFIG="+path, checkMainEnvKey+"="+checkMainValueKey)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		t.Fatalf("main() --check-config error = %v; stderr = %q; stdout = %q", err, stderr.String(), stdout.String())
	}

	output := stdout.String()
	for _, want := range []string{
		"config",
		"OK",
		path,
		"provider test (chat-completions)",
		"result   PASS (1 checked, 0 skipped, 0 failed)",
	} {
		if !strings.Contains(output, want) {
			t.Fatalf("main() --check-config output = %q, want it to contain %q", output, want)
		}
	}
	if strings.Contains(output, checkMainValueKey) {
		t.Fatalf("main() --check-config output leaked the API key value: %q", output)
	}
}

func TestMainCheckConfigMissingPath(t *testing.T) {
	if os.Getenv(mainChild) == t.Name() {
		os.Args = []string{os.Args[0], "--check-config", os.Getenv(checkMainPathEnv)}
		main()
		os.Exit(0)
	}

	missing := filepath.Join(t.TempDir(), "missing.toml")
	cmd := newMainChildCommand(t)
	cmd.Env = append(cmd.Env, checkMainPathEnv+"="+missing)
	output, err := cmd.CombinedOutput()

	var exitErr *exec.ExitError
	if !errors.As(err, &exitErr) || exitErr.ExitCode() != 1 {
		t.Fatalf("main() --check-config missing path error = %v, want exit code 1; output:\n%s", err, output)
	}
	for _, want := range []string{"config", missing, "FAIL"} {
		if !strings.Contains(string(output), want) {
			t.Fatalf("main() --check-config missing path output = %q, want it to contain %q", output, want)
		}
	}
}

func TestMainCheckConfigScrubsNativeProviderKeyFromCodex(t *testing.T) {
	if os.Getenv(mainChild) == t.Name() {
		os.Args = []string{os.Args[0], "--check-config"}
		main()
		os.Exit(0)
	}

	fake := testutil.NewFakeChat(t, nil)
	fake.SetModels([]string{"test-model"})
	envFile := filepath.Join(t.TempDir(), "codex-env")
	contents := fmt.Sprintf(`
[providers.codex]
api = "codex-app-server"
command = %q
default_model = "gpt-6-astra"
models = [{ id = "gpt-6-astra" }]

[providers.native]
api = "chat-completions"
base_url = %q
env_key = %q
default_model = "test-model"
models = [{ id = "test-model" }]
`, os.Args[0], fake.URL, checkCodexEnvKey)
	path := writeConfigFile(t, contents)

	cmd := newMainChildCommand(t)
	cmd.Env = append(cmd.Env,
		"SUBAGENT_MCP_CONFIG="+path,
		checkCodexEnvKey+"=sk-codex-native-secret",
		"SUBAGENT_FAKE_CODEX=1",
		checkCodexEnvFile+"="+envFile,
	)
	output, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("main() --check-config error = %v; output:\n%s", err, output)
	}

	data, err := os.ReadFile(envFile)
	if err != nil {
		t.Fatalf("os.ReadFile(%q) error = %v; check-config output:\n%s", envFile, err, output)
	}
	names := strings.Split(string(data), "\n")
	present := make(map[string]bool, len(names))
	for _, name := range names {
		present[name] = true
	}
	if present[checkCodexEnvKey] {
		t.Fatalf("codex app-server inherited native provider key %q; environment names:\n%s", checkCodexEnvKey, data)
	}
	if !present["SUBAGENT_FAKE_CODEX"] {
		t.Fatalf("codex app-server environment is missing positive-control marker; environment names:\n%s", data)
	}
}

func TestMainRejectsInvalidToolName(t *testing.T) {
	if runMainChild(t) {
		return
	}

	path := writeConfigFile(t, validTestConfig)
	output := runMainExpectingFailure(t,
		"SUBAGENT_MCP_CONFIG="+path,
		testEnvKey+"=sk-test",
		"SUBAGENT_MCP_TOOL_NAME=bad name",
	)
	if !strings.Contains(output, "SUBAGENT_MCP_TOOL_NAME") {
		t.Fatalf("main() failure output = %q, want it to mention %q", output, "SUBAGENT_MCP_TOOL_NAME")
	}
}

func runMainChild(t *testing.T) bool {
	if os.Getenv(mainChild) != t.Name() {
		return false
	}
	main()
	return true
}

func runMainExpectingFailure(t *testing.T, extraEnv ...string) string {
	t.Helper()
	cmd := newMainChildCommand(t)
	cmd.Env = append(cmd.Env, extraEnv...)
	output, err := cmd.CombinedOutput()
	if err == nil {
		t.Fatalf("main() accepted invalid configuration; child output:\n%s", output)
	}

	var exitErr *exec.ExitError
	if !errors.As(err, &exitErr) || exitErr.ExitCode() == 0 {
		t.Fatalf("main() error = %v, want a non-zero exit", err)
	}
	return string(output)
}

// newMainChildCommand re-executes this test binary as the named test in a child
// process with a clean HOME and no inherited subagent env.
func newMainChildCommand(t *testing.T) *exec.Cmd {
	t.Helper()
	home := t.TempDir()

	cmd := exec.Command(os.Args[0], "-test.run=^"+regexp.QuoteMeta(t.Name())+"$")
	for _, entry := range os.Environ() {
		if !hasEnvName(entry, "HOME") &&
			!hasEnvName(entry, mainChild) &&
			!hasEnvName(entry, "SUBAGENT_MCP_CONFIG") &&
			!hasEnvName(entry, "SUBAGENT_MCP_TOOL_NAME") &&
			!hasEnvName(entry, testEnvKey) &&
			!hasEnvName(entry, checkMainEnvKey) &&
			!hasEnvName(entry, checkMainPathEnv) {
			cmd.Env = append(cmd.Env, entry)
		}
	}
	cmd.Env = append(cmd.Env, "HOME="+home, mainChild+"="+t.Name())
	return cmd
}

func writeConfigFile(t *testing.T, contents string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(contents), 0o600); err != nil {
		t.Fatalf("os.WriteFile() error = %v", err)
	}
	return path
}

func hasEnvName(entry, name string) bool {
	return strings.HasPrefix(entry, name+"=")
}

// TestMainStopsOnSignal pins the shutdown path end to end: a serving process must
// stop on SIGTERM, release what it holds, and report success rather than the
// cancellation mcp-go ends its serve loop with.
//
// The child is driven through an MCP initialize exchange first, so the signal
// cannot arrive before the server installed its handler: a fixed sleep would
// make the test fail with "signal: terminated" whenever a loaded machine needs
// longer than the sleep to start.
func TestMainStopsOnSignal(t *testing.T) {
	if runMainChild(t) {
		return
	}

	path := writeConfigFile(t, validTestConfig)
	cmd := newMainChildCommand(t)
	cmd.Env = append(cmd.Env, "SUBAGENT_MCP_CONFIG="+path)

	stdin, err := cmd.StdinPipe()
	if err != nil {
		t.Fatalf("StdinPipe() error = %v", err)
	}
	defer stdin.Close()
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatalf("StdoutPipe() error = %v", err)
	}
	var stderr bytes.Buffer
	cmd.Stderr = &stderr

	if err := cmd.Start(); err != nil {
		t.Fatalf("start child: %v", err)
	}
	select {
	case err := <-serveUntilInitialized(cmd, stdin, stdout):
		if err != nil {
			t.Fatalf("child never answered initialize: %v; stderr = %q", err, stderr.String())
		}
	case <-time.After(20 * time.Second):
		_ = cmd.Process.Kill()
		t.Fatalf("child never answered initialize; stderr = %q", stderr.String())
	}

	// A stdout pipe that stays open keeps the child's writes from failing while
	// it shuts down; the readiness answer has already been read.
	start := time.Now()
	if err := cmd.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatalf("signal child: %v", err)
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("main() exit after SIGTERM = %v, want success; stderr = %q", err, stderr.String())
		}
	case <-time.After(20 * time.Second):
		_ = cmd.Process.Kill()
		t.Fatalf("main() did not stop within 20s of SIGTERM; stderr = %q", stderr.String())
	}

	// Shutdown is expected to be immediate; a run that waits out the ten-second
	// grace period instead would still pass a bare liveness check.
	if elapsed := time.Since(start); elapsed > 8*time.Second {
		t.Errorf("main() took %v to stop, want it to stop without waiting out the grace period", elapsed)
	}
}

// serveUntilInitialized writes one MCP initialize request and reports when the
// server has answered it, which is the first moment the process is certainly
// serving with its signal handler installed.
func serveUntilInitialized(cmd *exec.Cmd, stdin io.Writer, stdout io.Reader) <-chan error {
	result := make(chan error, 1)
	go func() {
		request := `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"main-test","version":"1"}}}` + "\n"
		if _, err := io.WriteString(stdin, request); err != nil {
			result <- err
			return
		}
		reader := bufio.NewReader(stdout)
		if _, err := reader.ReadString('\n'); err != nil {
			result <- err
			return
		}
		result <- nil
	}()
	return result
}
