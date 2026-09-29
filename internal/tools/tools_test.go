package tools

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/policy"
	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func TestRunShell(t *testing.T) {
	t.Run("runs command in working directory", func(t *testing.T) {
		cwd := t.TempDir()

		out, exitCode, err := RunShell(context.Background(), cwd, "echo hello; pwd", time.Second)
		if err != nil {
			t.Fatalf("RunShell() error = %v", err)
		}
		if exitCode != 0 {
			t.Fatalf("RunShell() exitCode = %d, want 0", exitCode)
		}

		lines := strings.Split(strings.TrimSpace(out), "\n")
		if len(lines) != 2 || lines[0] != "hello" {
			t.Fatalf("RunShell() output = %q, want hello followed by cwd", out)
		}

		wantCWD, err := filepath.EvalSymlinks(cwd)
		if err != nil {
			t.Fatalf("EvalSymlinks(%q): %v", cwd, err)
		}
		gotCWD, err := filepath.EvalSymlinks(lines[1])
		if err != nil {
			t.Fatalf("EvalSymlinks(%q): %v", lines[1], err)
		}
		if gotCWD != wantCWD {
			t.Errorf("pwd = %q, want %q", gotCWD, wantCWD)
		}
	})

	t.Run("returns non-zero exit code without error", func(t *testing.T) {
		out, exitCode, err := RunShell(context.Background(), t.TempDir(), "exit 3", time.Second)
		if err != nil {
			t.Fatalf("RunShell() error = %v", err)
		}
		if exitCode != 3 {
			t.Errorf("RunShell() exitCode = %d, want 3", exitCode)
		}
		if out != "" {
			t.Errorf("RunShell() output = %q, want empty", out)
		}
	})

	t.Run("merges stdout and stderr", func(t *testing.T) {
		out, exitCode, err := RunShell(context.Background(), t.TempDir(), "echo out; echo err 1>&2", time.Second)
		if err != nil {
			t.Fatalf("RunShell() error = %v", err)
		}
		if exitCode != 0 {
			t.Fatalf("RunShell() exitCode = %d, want 0", exitCode)
		}
		if !strings.Contains(out, "out") || !strings.Contains(out, "err") {
			t.Errorf("RunShell() output = %q, want stdout and stderr", out)
		}
	})

	t.Run("kills process group on timeout", func(t *testing.T) {
		start := time.Now()
		out, exitCode, err := RunShell(context.Background(), t.TempDir(), "sleep 5", 100*time.Millisecond)
		elapsed := time.Since(start)

		if err == nil || !strings.Contains(err.Error(), "timed out") {
			t.Fatalf("RunShell() error = %v, want timed out error", err)
		}
		if exitCode != -1 {
			t.Errorf("RunShell() exitCode = %d, want -1", exitCode)
		}
		if out != "" {
			t.Errorf("RunShell() output = %q, want empty", out)
		}
		if elapsed >= time.Second {
			t.Errorf("RunShell() elapsed = %v, want less than 1s", elapsed)
		}
	})

	t.Run("returns success when a background process holds the pipe", func(t *testing.T) {
		cwd := t.TempDir()
		command := "sleep 5 & echo $! > child.pid; echo done"

		start := time.Now()
		out, exitCode, err := RunShell(context.Background(), cwd, command, 200*time.Millisecond)
		elapsed := time.Since(start)

		childPID := readPIDFile(t, filepath.Join(cwd, "child.pid"))
		t.Cleanup(func() {
			if killErr := syscall.Kill(childPID, syscall.SIGKILL); killErr != nil && !errors.Is(killErr, syscall.ESRCH) {
				t.Errorf("kill background process %d: %v", childPID, killErr)
			}
		})

		if err != nil {
			t.Fatalf("RunShell() error = %v", err)
		}
		if exitCode != 0 {
			t.Errorf("RunShell() exitCode = %d, want 0", exitCode)
		}
		if !strings.Contains(out, "done") {
			t.Errorf("RunShell() output = %q, want done", out)
		}
		if elapsed >= 2*time.Second {
			t.Errorf("RunShell() elapsed = %v, want less than 2s", elapsed)
		}
		if killErr := syscall.Kill(childPID, 0); killErr != nil {
			t.Errorf("background process %d did not outlive bash: %v", childPID, killErr)
		}
	})

	t.Run("kills a grandchild and bounds inherited pipe wait", func(t *testing.T) {
		cwd := t.TempDir()
		command := "sleep 5 & echo $! > child.pid; setsid sleep 5 & echo $! > escaped.pid; wait"

		start := time.Now()
		out, exitCode, err := RunShell(context.Background(), cwd, command, 200*time.Millisecond)
		elapsed := time.Since(start)

		childPID := readPIDFile(t, filepath.Join(cwd, "child.pid"))
		escapedPID := readPIDFile(t, filepath.Join(cwd, "escaped.pid"))
		t.Cleanup(func() {
			if killErr := syscall.Kill(escapedPID, syscall.SIGKILL); killErr != nil && !errors.Is(killErr, syscall.ESRCH) {
				t.Errorf("kill escaped descendant %d: %v", escapedPID, killErr)
			}
		})

		if err == nil || !strings.Contains(err.Error(), "timed out") {
			t.Fatalf("RunShell() error = %v, want timed out error", err)
		}
		if exitCode != -1 {
			t.Errorf("RunShell() exitCode = %d, want -1", exitCode)
		}
		if out != "" {
			t.Errorf("RunShell() output = %q, want empty", out)
		}
		if elapsed >= 2*time.Second {
			t.Errorf("RunShell() elapsed = %v, want less than 2s", elapsed)
		}
		if !waitForProcessExit(childPID, time.Second) {
			t.Errorf("process-group grandchild %d is still alive", childPID)
		}
	})

	t.Run("truncates output", func(t *testing.T) {
		out, exitCode, err := RunShell(
			context.Background(),
			t.TempDir(),
			"head -c 20000 /dev/zero | tr '\\0' 'a'",
			time.Second,
		)
		if err != nil {
			t.Fatalf("RunShell() error = %v", err)
		}
		if exitCode != 0 {
			t.Fatalf("RunShell() exitCode = %d, want 0", exitCode)
		}
		if len(out) <= MaxOutputBytes || len(out) >= MaxOutputBytes+100 {
			t.Errorf("len(RunShell() output) = %d, want slightly more than %d", len(out), MaxOutputBytes)
		}
		const marker = "[output truncated: 20000 bytes total]"
		if !strings.HasSuffix(out, marker) {
			t.Errorf("RunShell() output suffix = %q, want %q", out[len(out)-len(marker):], marker)
		}
	})
}

func readPIDFile(t *testing.T, path string) int {
	t.Helper()

	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile(%q): %v", path, err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
	if err != nil {
		t.Fatalf("parse PID from %q: %v", path, err)
	}
	return pid
}

func TestScrubbedEnvConfigurable(t *testing.T) {
	t.Cleanup(func() { SetScrubbedEnv(nil, []string{"SUBAGENT_MCP_"}) })
	SetScrubbedEnv([]string{"MY_PROVIDER_KEY"}, []string{"SUBAGENT_MCP_"})
	t.Setenv("MY_PROVIDER_KEY", "secret-1")
	t.Setenv("SUBAGENT_MCP_CONFIG", "/x")
	t.Setenv("KEEP_ME", "kept")
	out, _, err := RunShell(context.Background(), t.TempDir(), "env", 5*time.Second)
	if err != nil || strings.Contains(out, "secret-1") || strings.Contains(out, "SUBAGENT_MCP_CONFIG") || !strings.Contains(out, "KEEP_ME=kept") {
		t.Fatalf("env output = %q, err = %v", out, err)
	}
}

func TestScrubbedEnvSandboxed(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		testutil.LandlockUnavailable(t, err)
	}
	t.Cleanup(func() { SetScrubbedEnv(nil, []string{"SUBAGENT_MCP_"}) })
	SetScrubbedEnv([]string{"MY_PROVIDER_KEY"}, []string{"SUBAGENT_MCP_"})
	t.Setenv("MY_PROVIDER_KEY", "secret-2")
	cwd := t.TempDir()
	out, _, err := RunShellSandboxed(context.Background(), cwd, "env", 5*time.Second, []string{cwd})
	if err != nil || strings.Contains(out, "secret-2") {
		t.Fatalf("env output = %q, err = %v", out, err)
	}
}

func TestAutoAllowedGitCommandsDoNotRunRepositoryPrograms(t *testing.T) {
	repo := t.TempDir()
	home := t.TempDir()
	markerRoot := t.TempDir()
	t.Setenv("TMPDIR", markerRoot)
	marker := filepath.Join(markerRoot, "marker")
	hook := filepath.Join(markerRoot, "hook.sh")
	writeGitHook(t, hook, marker)
	t.Setenv("GIT_EXTERNAL_DIFF", hook)
	t.Setenv("GIT_PAGER", hook)
	t.Setenv("PAGER", hook)
	t.Setenv("GIT_CONFIG_COUNT", "1")
	t.Setenv("GIT_CONFIG_KEY_0", "core.fsmonitor")
	t.Setenv("GIT_CONFIG_VALUE_0", hook)
	t.Setenv("GIT_CONFIG_PARAMETERS", "'core.fsmonitor'='"+hook+"'")

	gitRun(t, home, repo, "init", "-q")
	gitRun(t, home, repo, "config", "user.email", "test@example.com")
	gitRun(t, home, repo, "config", "user.name", "Test")
	if err := os.WriteFile(filepath.Join(repo, "file.txt"), []byte("one\n"), 0o600); err != nil {
		t.Fatalf("write initial file: %v", err)
	}
	gitRun(t, home, repo, "add", "file.txt")
	gitRun(t, home, repo, "commit", "-qm", "initial")
	if err := os.WriteFile(filepath.Join(repo, "file.txt"), []byte("two\n"), 0o600); err != nil {
		t.Fatalf("write changed file: %v", err)
	}
	if err := os.WriteFile(filepath.Join(repo, ".gitattributes"), []byte("file.txt diff=hostile filter=hostile\n"), 0o600); err != nil {
		t.Fatalf("write attributes: %v", err)
	}
	gitRun(t, home, repo, "add", ".gitattributes")
	gitRun(t, home, repo, "commit", "-qm", "attributes")
	if err := os.WriteFile(filepath.Join(repo, "file.txt"), []byte("three\n"), 0o600); err != nil {
		t.Fatalf("write second changed file: %v", err)
	}

	for _, tc := range []struct {
		name    string
		command string
		config  []string
	}{
		{name: "diff external", command: "git diff HEAD~1 HEAD", config: []string{"diff.external", hook}},
		{name: "textconv", command: "git diff HEAD -- file.txt", config: []string{"--unset", "diff.external"}},
		{name: "fsmonitor", command: "git status", config: []string{"core.fsmonitor", hook}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			commands := []string{
				tc.command,
				"git status",
				"git diff HEAD -- file.txt",
				"git log -p -1",
				"git show HEAD",
				"git blame file.txt",
				"git branch -a",
				"git rev-parse HEAD",
				"git ls-files",
			}
			configArgs := append([]string{"config"}, tc.config...)
			gitRun(t, home, repo, configArgs...)
			if tc.name == "diff external" {
				gitRun(t, home, repo, "config", "alias.diff", "!"+hook)
			}
			gitRun(t, home, repo, "config", "alias.config", "!"+hook)
			if tc.name == "textconv" {
				gitRun(t, home, repo, "config", "diff.hostile.textconv", hook)
				gitRun(t, home, repo, "config", "filter.hostile.clean", hook)
				gitRun(t, home, repo, "config", "filter.hostile.smudge", "cat")
			}
			if tc.name == "fsmonitor" {
				gitRun(t, home, repo, "config", "alias.status", "!"+hook)
			}

			for _, command := range commands {
				if decision, reason := policy.Evaluate(
					policy.Sandbox("read-only"),
					policy.ApprovalPolicy("untrusted"),
					policy.Request{Tool: "shell", Command: command, Cwd: repo},
				); decision != policy.Allow {
					t.Fatalf("policy rejected %q: %v (%s)", command, decision, reason)
				}
			}

			for _, runner := range []struct {
				name string
				run  func(string) (string, int, error)
			}{
				{name: "native", run: func(command string) (string, int, error) {
					return RunShell(context.Background(), repo, command, 10*time.Second)
				}},
				{name: "landlock", run: func(command string) (string, int, error) {
					return RunShellSandboxed(context.Background(), repo, command, 10*time.Second, []string{markerRoot})
				}},
			} {
				t.Run(runner.name, func(t *testing.T) {
					if runner.name == "landlock" {
						if err := sandbox.Available(); err != nil {
							testutil.LandlockUnavailable(t, err)
						}
					}
					for _, command := range commands {
						_ = os.Remove(marker)
						out, code, err := func() (string, int, error) {
							return runner.run(command)
						}()
						if err != nil || code != 0 {
							t.Fatalf("%s runner %q = (%q, %d, %v)", runner.name, command, out, code, err)
						}
						if _, err := os.Stat(marker); err == nil {
							t.Fatalf("%s runner executed repository-configured program for %q; output=%q", runner.name, command, out)
						} else if !errors.Is(err, os.ErrNotExist) {
							t.Fatalf("stat marker after %s runner %q: %v", runner.name, command, err)
						}
					}

					_ = os.Remove(marker)
					plainGit(t, home, repo, tc.command)
					if _, err := os.Stat(marker); err != nil {
						t.Fatalf("plain git did not execute repository-configured program: %v", err)
					}
				})
			}
		})
	}
}

func TestRunShellGitFunctionDoesNotBreakCommit(t *testing.T) {
	repo := t.TempDir()
	home := t.TempDir()
	gitRun(t, home, repo, "init", "-q")
	gitRun(t, home, repo, "config", "user.email", "test@example.com")
	gitRun(t, home, repo, "config", "user.name", "Test")
	if err := os.WriteFile(filepath.Join(repo, "file.txt"), []byte("content\n"), 0o600); err != nil {
		t.Fatalf("write file: %v", err)
	}

	out, code, err := RunShell(context.Background(), repo, "git add file.txt && git commit -qm initial", 10*time.Second)
	if err != nil || code != 0 {
		t.Fatalf("RunShell() = (%q, %d, %v), want a successful commit", out, code, err)
	}
	if _, err := os.Stat(filepath.Join(repo, ".git", "objects")); err != nil {
		t.Fatalf("commit did not create git objects: %v", err)
	}
}

func writeGitHook(t *testing.T, hook, marker string) {
	t.Helper()
	content := "#!/bin/sh\nprintf 'triggered\\n' > " + shellQuote(marker) + "\n"
	if err := os.WriteFile(hook, []byte(content), 0o700); err != nil {
		t.Fatalf("write git hook: %v", err)
	}
}

func gitRun(t *testing.T, home, repo string, args ...string) {
	t.Helper()
	cmd := exec.Command("git", args...)
	cmd.Dir = repo
	cmd.Env = gitTestEnv(home)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("git %s: %v\n%s", strings.Join(args, " "), err, out)
	}
}

func plainGit(t *testing.T, home, repo, command string) {
	t.Helper()
	fields := strings.Fields(command)
	if len(fields) < 2 || fields[0] != "git" {
		t.Fatalf("plainGit got non-git command %q", command)
	}
	cmd := exec.Command("git", fields[1:]...)
	cmd.Dir = repo
	cmd.Env = gitTestEnv(home)
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("plain git %s: %v\n%s", strings.Join(fields[1:], " "), err, out)
	}
}

func gitTestEnv(home string) []string {
	env := make([]string, 0, len(os.Environ())+3)
	for _, entry := range os.Environ() {
		name, _, _ := strings.Cut(entry, "=")
		switch name {
		case "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0", "GIT_CONFIG_PARAMETERS", "GIT_EXTERNAL_DIFF", "GIT_PAGER", "PAGER":
			continue
		}
		env = append(env, entry)
	}
	env = append(env,
		"HOME="+home,
		"GIT_CONFIG_GLOBAL="+filepath.Join(home, "global-config"),
		"GIT_CONFIG_SYSTEM="+filepath.Join(home, "system-config"),
		"GIT_PAGER=cat",
		"PAGER=cat",
	)
	return env
}

func shellQuote(value string) string {
	return "'" + strings.ReplaceAll(value, "'", "'\\''") + "'"
}

func TestRunShellScrubsLoginProfileProviderKeyAndKeepsPATH(t *testing.T) {
	for _, tc := range []struct {
		name    string
		sandbox bool
	}{
		{name: "native"},
		{name: "sandboxed", sandbox: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if tc.sandbox {
				if err := sandbox.Available(); err != nil {
					testutil.LandlockUnavailable(t, err)
				}
			}

			const keyName = "PROFILE_PROVIDER_KEY"
			const keyValue = "profile-provider-secret"
			home := t.TempDir()
			bin := t.TempDir()
			toolPath := filepath.Join(bin, "profile-tool")
			if err := os.WriteFile(toolPath, []byte("#!/bin/sh\nprintf 'profile-tool-found\\n'\n"), 0o755); err != nil {
				t.Fatalf("write profile tool: %v", err)
			}
			profile := fmt.Sprintf("export %s=%s\nexport PATH='%s':$PATH\n", keyName, keyValue, strings.ReplaceAll(bin, "'", "'\\''"))
			if err := os.WriteFile(filepath.Join(home, ".bash_profile"), []byte(profile), 0o600); err != nil {
				t.Fatalf("write bash profile: %v", err)
			}

			t.Setenv("HOME", home)
			SetScrubbedEnv([]string{keyName}, []string{"SUBAGENT_MCP_"})
			t.Cleanup(func() { SetScrubbedEnv(nil, []string{"SUBAGENT_MCP_"}) })

			command := `printf 'key=%s\n' "${PROFILE_PROVIDER_KEY-unset}"; profile-tool`
			var out string
			var err error
			if tc.sandbox {
				out, _, err = RunShellSandboxed(context.Background(), t.TempDir(), command, 5*time.Second, []string{t.TempDir()})
			} else {
				out, _, err = RunShell(context.Background(), t.TempDir(), command, 5*time.Second)
			}
			if err != nil {
				t.Fatalf("shell error = %v; output = %q", err, out)
			}
			if strings.Contains(out, keyValue) || !strings.Contains(out, "key=unset\n") {
				t.Fatalf("shell saw profile key: %q", out)
			}
			if !strings.Contains(out, "profile-tool-found") {
				t.Fatalf("shell did not find profile-provided PATH executable: %q", out)
			}
		})
	}
}

func TestRunShellScrubsLoginProfileSubagentEnvironment(t *testing.T) {
	for _, tc := range []struct {
		name    string
		sandbox bool
	}{
		{name: "native"},
		{name: "sandboxed", sandbox: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if tc.sandbox {
				if err := sandbox.Available(); err != nil {
					testutil.LandlockUnavailable(t, err)
				}
			}

			home := t.TempDir()
			if err := os.WriteFile(filepath.Join(home, ".bash_profile"), []byte("export SUBAGENT_MCP_PROFILE_SECRET=profile-subagent-secret\n"), 0o600); err != nil {
				t.Fatalf("write bash profile: %v", err)
			}
			t.Setenv("HOME", home)
			SetScrubbedEnv(nil, []string{"SUBAGENT_MCP_"})
			t.Cleanup(func() { SetScrubbedEnv(nil, []string{"SUBAGENT_MCP_"}) })

			command := `printf 'subagent=%s\n' "${SUBAGENT_MCP_PROFILE_SECRET-unset}"`
			var out string
			var err error
			if tc.sandbox {
				out, _, err = RunShellSandboxed(context.Background(), t.TempDir(), command, 5*time.Second, []string{t.TempDir()})
			} else {
				out, _, err = RunShell(context.Background(), t.TempDir(), command, 5*time.Second)
			}
			if err != nil {
				t.Fatalf("shell error = %v; output = %q", err, out)
			}
			if out != "subagent=unset\n" {
				t.Fatalf("shell saw profile SUBAGENT_MCP_ variable: %q", out)
			}
		})
	}
}

func TestRunShellDisablesProfileAliasesAndFunctions(t *testing.T) {
	home := t.TempDir()
	profile := "shopt -s expand_aliases\nalias echo='printf alias\\n'\nfunction echo { printf 'function\\n'; }\n"
	if err := os.WriteFile(filepath.Join(home, ".bash_profile"), []byte(profile), 0o600); err != nil {
		t.Fatalf("write bash profile: %v", err)
	}
	t.Setenv("HOME", home)

	out, code, err := RunShell(context.Background(), t.TempDir(), "echo safe", 5*time.Second)
	if err != nil || code != 0 {
		t.Fatalf("RunShell() = (%q, %d, %v)", out, code, err)
	}
	if out != "safe\n" {
		t.Fatalf("RunShell() output = %q, want the builtin echo output", out)
	}
}

func waitForProcessExit(pid int, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for {
		err := syscall.Kill(pid, 0)
		if errors.Is(err, syscall.ESRCH) {
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestReadFile(t *testing.T) {
	cwd := t.TempDir()

	t.Run("resolves relative path", func(t *testing.T) {
		if err := os.WriteFile(filepath.Join(cwd, "example.txt"), []byte("hello"), 0o644); err != nil {
			t.Fatalf("WriteFile fixture: %v", err)
		}

		got, err := ReadFile(context.Background(), cwd, "example.txt")
		if err != nil {
			t.Fatalf("ReadFile() error = %v", err)
		}
		if got != "hello" {
			t.Errorf("ReadFile() = %q, want %q", got, "hello")
		}
	})

	t.Run("truncates large file", func(t *testing.T) {
		const size = 20000
		path := filepath.Join(cwd, "large.txt")
		if err := os.WriteFile(path, []byte(strings.Repeat("a", size)), 0o644); err != nil {
			t.Fatalf("WriteFile fixture: %v", err)
		}

		got, err := ReadFile(context.Background(), cwd, "large.txt")
		if err != nil {
			t.Fatalf("ReadFile() error = %v", err)
		}
		if len(got) <= MaxOutputBytes || len(got) >= MaxOutputBytes+100 {
			t.Errorf("len(ReadFile()) = %d, want slightly more than %d", len(got), MaxOutputBytes)
		}
		const marker = "[content truncated: 20000 bytes total; continue with offset=2]"
		if !strings.HasSuffix(got, marker) {
			t.Errorf("ReadFile() result suffix = %q, want %q", got[len(got)-len(marker):], marker)
		}
	})

	t.Run("bounds read of sparse large file", func(t *testing.T) {
		const size int64 = 256 * 1024 * 1024
		path := filepath.Join(cwd, "sparse-large.txt")
		file, err := os.Create(path)
		if err != nil {
			t.Fatalf("Create fixture: %v", err)
		}
		if err := file.Truncate(size); err != nil {
			_ = file.Close()
			t.Fatalf("Truncate fixture: %v", err)
		}
		if err := file.Close(); err != nil {
			t.Fatalf("Close fixture: %v", err)
		}

		start := time.Now()
		got, err := ReadFile(context.Background(), cwd, "sparse-large.txt")
		elapsed := time.Since(start)

		if err != nil {
			t.Fatalf("ReadFile() error = %v", err)
		}
		const marker = "[content truncated: 268435456 bytes total; continue with offset=2]"
		if !strings.HasSuffix(got, marker) {
			t.Errorf("ReadFile() result does not end with %q", marker)
		}
		if elapsed >= time.Second {
			t.Errorf("ReadFile() elapsed = %v, want less than 1s", elapsed)
		}
	})

	t.Run("times out reading FIFO without writer", func(t *testing.T) {
		path := filepath.Join(cwd, "blocked.fifo")
		if err := syscall.Mkfifo(path, 0o600); err != nil {
			t.Fatalf("Mkfifo fixture: %v", err)
		}

		ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
		defer cancel()
		start := time.Now()
		_, err := ReadFile(ctx, cwd, "blocked.fifo")
		elapsed := time.Since(start)

		if err == nil || !strings.Contains(err.Error(), "timed out") {
			t.Fatalf("ReadFile() error = %v, want timed out error", err)
		}
		if elapsed >= time.Second {
			t.Errorf("ReadFile() elapsed = %v, want less than 1s", elapsed)
		}
	})

	t.Run("returns error for nonexistent file", func(t *testing.T) {
		if _, err := ReadFile(context.Background(), cwd, "missing.txt"); err == nil {
			t.Fatal("ReadFile() error = nil, want an error")
		}
	})
}

func TestReadFileRange(t *testing.T) {
	cwd := t.TempDir()
	const content = "l1\nl2\nl3\nl4\nl5\n"
	if err := os.WriteFile(filepath.Join(cwd, "lines.txt"), []byte(content), 0o644); err != nil {
		t.Fatalf("WriteFile fixture: %v", err)
	}

	tests := []struct {
		name   string
		offset int
		limit  int
		want   string
	}{
		{
			name:   "line limit stops before end of file",
			offset: 2,
			limit:  2,
			want:   "l2\nl3\n\n[more lines follow; continue with offset=4]",
		},
		{
			name:   "offset to last lines",
			offset: 4,
			limit:  0,
			want:   "l4\nl5\n",
		},
		{
			name:   "zero offset reads whole file",
			offset: 0,
			limit:  0,
			want:   content,
		},
		{
			name:   "offset past end of file",
			offset: 9,
			limit:  0,
			want:   "[offset 9 is past the end of the file (5 lines)]",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ReadFileRange(context.Background(), cwd, "lines.txt", tt.offset, tt.limit)
			if err != nil {
				t.Fatalf("ReadFileRange(offset=%d, limit=%d) error = %v", tt.offset, tt.limit, err)
			}
			if got != tt.want {
				t.Errorf("ReadFileRange(offset=%d, limit=%d) = %q, want %q", tt.offset, tt.limit, got, tt.want)
			}
		})
	}
}

func TestReadFileRangeByteCapKeepsWholeLines(t *testing.T) {
	cwd := t.TempDir()
	var fileContent strings.Builder
	for i := 1; i <= 3000; i++ {
		fmt.Fprintf(&fileContent, "%09d\n", i)
	}
	if err := os.WriteFile(filepath.Join(cwd, "many-lines.txt"), []byte(fileContent.String()), 0o644); err != nil {
		t.Fatalf("WriteFile fixture: %v", err)
	}

	got, err := ReadFileRange(context.Background(), cwd, "many-lines.txt", 1, 0)
	if err != nil {
		t.Fatalf("ReadFileRange() error = %v", err)
	}

	const marker = "\n[content truncated: 30000 bytes total; continue with offset=1639]"
	if !strings.HasSuffix(got, marker) {
		t.Fatalf("ReadFileRange() does not end with %q", marker)
	}
	head := strings.TrimSuffix(got, marker)
	if len(head) > MaxOutputBytes {
		t.Errorf("content length = %d, want <= %d", len(head), MaxOutputBytes)
	}
	if !strings.HasSuffix(head, "\n") {
		t.Errorf("content does not end in a whole line: %q", head)
	}
	// 16384/10 = 1638 whole 10-byte lines fit in the cap.
	if want := 1638 * 10; len(head) != want {
		t.Errorf("content length = %d, want %d", len(head), want)
	}

	next, err := ReadFileRange(context.Background(), cwd, "many-lines.txt", 1639, 0)
	if err != nil {
		t.Fatalf("ReadFileRange(offset=1639) error = %v", err)
	}
	firstLine := fmt.Sprintf("%09d\n", 1639)
	if !strings.HasPrefix(next, firstLine) {
		t.Errorf("ReadFileRange(offset=1639) does not start with line 1639 (%q)", firstLine)
	}
}

func TestReadFileRangeSingleHugeLine(t *testing.T) {
	cwd := t.TempDir()
	if err := os.WriteFile(filepath.Join(cwd, "huge-line.txt"), []byte(strings.Repeat("a", 20000)), 0o644); err != nil {
		t.Fatalf("WriteFile fixture: %v", err)
	}

	got, err := ReadFileRange(context.Background(), cwd, "huge-line.txt", 1, 0)
	if err != nil {
		t.Fatalf("ReadFileRange() error = %v", err)
	}

	const marker = "\n[content truncated: 20000 bytes total; continue with offset=2]"
	if !strings.HasSuffix(got, marker) {
		t.Fatalf("ReadFileRange() does not end with %q", marker)
	}
	line := strings.TrimSuffix(got, marker)
	if len(line) != MaxOutputBytes {
		t.Errorf("content length = %d, want %d", len(line), MaxOutputBytes)
	}
	if line != strings.Repeat("a", MaxOutputBytes) {
		t.Errorf("content is not the first %d bytes of the line", MaxOutputBytes)
	}
}

func TestReadFileRefusesProtectedFiles(t *testing.T) {
	dir := t.TempDir()
	protected := filepath.Join(dir, "config.toml")
	if err := os.WriteFile(protected, []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { SetProtectedFiles(nil) })
	SetProtectedFiles([]string{protected})
	link := filepath.Join(t.TempDir(), "link.toml")
	if err := os.Symlink(protected, link); err != nil {
		t.Fatal(err)
	}
	for _, path := range []string{protected, link} {
		if _, err := ReadFile(context.Background(), dir, path); err == nil || !strings.Contains(err.Error(), "refusing to read") {
			t.Fatalf("ReadFile(%s) error = %v", path, err)
		}
	}
	if _, err := ReadFile(context.Background(), dir, "config.toml"); err == nil || !strings.Contains(err.Error(), "refusing to read") {
		t.Fatalf("relative ReadFile(config.toml) error = %v, want refusal", err)
	}
	other := filepath.Join(dir, "other.txt")
	if err := os.WriteFile(other, []byte("ok"), 0o600); err != nil {
		t.Fatal(err)
	}
	if got, err := ReadFile(context.Background(), dir, other); err != nil || got != "ok" {
		t.Fatalf("unrelated file = %q, %v", got, err)
	}
}

func TestReadFileRefusesRelativeProtectedPath(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "config.toml"), []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Chdir(dir)
	t.Cleanup(func() { SetProtectedFiles(nil) })
	SetProtectedFiles([]string{"config.toml"})
	if _, err := ReadFile(context.Background(), dir, "config.toml"); err == nil || !strings.Contains(err.Error(), "refusing to read") {
		t.Fatalf("ReadFile() error = %v, want refusal for a relative protected path", err)
	}
}

func TestWriteFile(t *testing.T) {
	cwd := t.TempDir()
	relativePath := filepath.Join("a", "b", "c.txt")
	absolutePath := filepath.Join(cwd, relativePath)

	if err := WriteFile(cwd, relativePath, "first"); err != nil {
		t.Fatalf("WriteFile() creating nested path: %v", err)
	}
	got, err := os.ReadFile(absolutePath)
	if err != nil {
		t.Fatalf("ReadFile(%q): %v", absolutePath, err)
	}
	if string(got) != "first" {
		t.Errorf("file content = %q, want %q", got, "first")
	}

	if err := WriteFile(cwd, relativePath, "second"); err != nil {
		t.Fatalf("WriteFile() overwriting file: %v", err)
	}
	got, err = os.ReadFile(absolutePath)
	if err != nil {
		t.Fatalf("ReadFile(%q): %v", absolutePath, err)
	}
	if string(got) != "second" {
		t.Errorf("overwritten file content = %q, want %q", got, "second")
	}
}

func TestRunShellSandboxed(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		testutil.LandlockUnavailable(t, err)
	}
	cwd := t.TempDir()
	outside := t.TempDir()

	out, code, err := RunShellSandboxed(context.Background(), cwd, "touch inside && echo ok", 10*time.Second, []string{cwd})
	if err != nil || code != 0 || !strings.Contains(out, "ok") {
		t.Fatalf("inside = (%q, %d, %v)", out, code, err)
	}

	out, code, err = RunShellSandboxed(context.Background(), cwd, "touch "+filepath.Join(outside, "bad"), 10*time.Second, []string{cwd})
	if err != nil || code == 0 {
		t.Fatalf("outside = (%q, %d, %v), want non-zero exit", out, code, err)
	}
	if _, statErr := os.Stat(filepath.Join(outside, "bad")); statErr == nil {
		t.Fatalf("outside file was created")
	}
}

func TestRunShellSandboxedTimeoutKillsCommand(t *testing.T) {
	if err := sandbox.Available(); err != nil {
		testutil.LandlockUnavailable(t, err)
	}
	start := time.Now()
	_, code, err := RunShellSandboxed(context.Background(), t.TempDir(), "sleep 5", 200*time.Millisecond, nil)
	if elapsed := time.Since(start); err == nil || code != -1 || elapsed > 2*time.Second {
		t.Fatalf("RunShellSandboxed() = (%d, %v) after %v, want timeout", code, err, elapsed)
	}
}
