package policy

import (
	"os"
	"path/filepath"
	"testing"
)

func TestIsWhitelistedShell(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name    string
		command string
		want    bool
	}{
		{name: "list", command: "ls -la", want: true},
		{name: "cat", command: "cat go.mod", want: true},
		{name: "ripgrep", command: "rg -n foo internal", want: true},
		{name: "git status", command: "git status", want: true},
		{name: "git diff", command: "git diff --stat", want: true},
		{name: "git log", command: "git log -5", want: true},
		{name: "git show", command: "git show HEAD", want: true},
		{name: "git branch", command: "git branch", want: true},
		{name: "git branch all", command: "git branch -a", want: true},
		{name: "git branch verbose", command: "git branch -v", want: true},
		{name: "git branch show current", command: "git branch --show-current", want: true},
		{name: "grep", command: "grep -r x .", want: true},
		{name: "find", command: "find . -name '*.go'", want: true},
		{name: "working directory", command: "pwd", want: true},
		{name: "head", command: "head -20 f", want: true},
		{name: "tail", command: "tail f", want: true},
		{name: "word count", command: "wc -l f", want: true},
		{name: "echo", command: "echo hi", want: true},
		{name: "whitelisted pipeline", command: "cat a | grep b", want: true},
		{name: "whitelisted sequence", command: "ls; pwd", want: true},
		{name: "remove", command: "rm -rf /", want: false},
		{name: "go test", command: "go test ./...", want: false},
		{name: "output redirection", command: "cat f > out", want: false},
		{name: "command substitution", command: "echo $(rm x)", want: false},
		{name: "backticks", command: "echo `rm x`", want: false},
		{name: "mixed and chain", command: "ls && rm x", want: false},
		{name: "mixed pipeline", command: "cat f | tee out", want: false},
		{name: "git write subcommand", command: "git push", want: false},
		{name: "leading assignment", command: "FOO=1 ls", want: false},
		{name: "find delete", command: "find . -delete", want: false},
		{name: "find exec", command: "find . -exec rm -rf {} +", want: false},
		{name: "ripgrep preprocessor", command: "rg --pre ./evil.sh foo .", want: false},
		{name: "git diff output file", command: "git diff --output=/tmp/x HEAD", want: false},
		{name: "git branch force delete", command: "git branch -D main", want: false},
		{name: "git branch rename", command: "git branch -m old new", want: false},
		{name: "git branch create", command: "git branch new-branch", want: false},
		{name: "git branch force move", command: "git branch -f main HEAD~5", want: false},
		{name: "empty", command: "", want: false},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Parallel()
			if got := isWhitelistedShell(tt.command); got != tt.want {
				t.Fatalf("isWhitelistedShell(%q) = %v, want %v", tt.command, got, tt.want)
			}
		})
	}
}

func TestIsWhitelistedShellLegitimateCommands(t *testing.T) {
	t.Parallel()

	commands := []string{
		"ls -la",
		"ls internal/",
		`rg "pattern" internal/`,
		`rg -n 'func Test' --glob '*.go'`,
		`grep -rn "foo bar" .`,
		`cat 'file with space.txt'`,
		"head -n 20 a.go",
		`find . -name '*.go'`,
		`find . -type f -name "*_test.go"`,
		"wc -l a b",
		"git status",
		"git log --oneline -5",
		"git diff --stat",
		"git rev-parse HEAD",
		"git branch -a",
		"rg foo | head",
		`rg "a;b" .`,
		`echo 'a;b'; pwd`,
	}

	for _, command := range commands {
		command := command
		t.Run(command, func(t *testing.T) {
			t.Parallel()
			if !isWhitelistedShell(command) {
				t.Fatalf("isWhitelistedShell(%q) = false, want true", command)
			}
		})
	}
}

func TestIsWhitelistedShellAdversarialCommands(t *testing.T) {
	t.Parallel()

	commands := []string{
		"find . '-delete'",
		`find . "-delete"`,
		"find . -de''lete",
		`rg '--pre=sh' x .`,
		`grep '--pre=sh' x .`,
		`echo ${x:=$'\x24(touch pwned)'}; echo ${x@P}`,
		"cat /proc/$PPID/environ",
		"echo $HOME",
		"ls $(pwd)",
		"ls `pwd`",
		"cat <(ls)",
		"cat >(ls)",
		"ls > out",
		"ls < in",
		"cd internal",
		"ls *",
		"git diff --stat *",
		"ls ~",
		"ls {--output=x}",
		"ls && rm x",
		"ls\nrm -rf x",
		"git -c core.pager=x log",
		"git --exec-path",
		"env FOO=1 ls",
		"rg --pre=sh x",
		"rg --pre x",
		"rg --pre-glob x",
		"rg --hostname-bin x",
		"grep --pre=sh x",
		"grep --pre x",
		"grep --pre-glob x",
		"grep --hostname-bin x",
		"git diff --output=x",
		"git diff --output x",
		"git diff -o x",
		"git show --output=x",
		"git show --output x",
		"git log --output=x",
		"git log --output x",
		"git log --show-signature",
		"git diff --ext-diff",
		"git show --textconv",
		"find . -fprint0 output",
		"find . -okdir echo file \\;",
		"find . -ok echo file \\;",
		"find . -fls output",
		"find . -fprintf output %p",
	}

	for _, command := range commands {
		command := command
		t.Run(command, func(t *testing.T) {
			t.Parallel()
			if isWhitelistedShell(command) {
				t.Fatalf("isWhitelistedShell(%q) = true, want false", command)
			}
		})
	}
}

func TestPathInside(t *testing.T) {
	t.Parallel()

	cwd := t.TempDir()
	outside := t.TempDir()
	insideFile := filepath.Join(cwd, "inside.txt")
	if err := os.WriteFile(insideFile, []byte("inside"), 0o600); err != nil {
		t.Fatalf("create inside fixture: %v", err)
	}

	directoryLink := filepath.Join(cwd, "outside-directory")
	if err := os.Symlink(outside, directoryLink); err != nil {
		t.Fatalf("create directory symlink: %v", err)
	}
	if err := os.WriteFile(filepath.Join(directoryLink, "escaped.txt"), []byte("escaped"), 0o600); err != nil {
		t.Fatalf("create file through directory symlink: %v", err)
	}

	outsideFile := filepath.Join(outside, "outside.txt")
	if err := os.WriteFile(outsideFile, []byte("outside"), 0o600); err != nil {
		t.Fatalf("create outside fixture: %v", err)
	}
	fileLink := filepath.Join(cwd, "outside-file")
	if err := os.Symlink(outsideFile, fileLink); err != nil {
		t.Fatalf("create file symlink: %v", err)
	}

	insideDirectory := filepath.Join(cwd, "inside-directory")
	if err := os.Mkdir(insideDirectory, 0o700); err != nil {
		t.Fatalf("create inside directory: %v", err)
	}
	insideDirectoryLink := filepath.Join(cwd, "inside-directory-link")
	if err := os.Symlink(insideDirectory, insideDirectoryLink); err != nil {
		t.Fatalf("create inside directory symlink: %v", err)
	}

	dotDotEscape := directoryLink + string(filepath.Separator) + ".." + string(filepath.Separator) + "evil"

	tests := []struct {
		name   string
		target string
		want   bool
	}{
		{name: "relative path inside cwd", target: "inside.txt", want: true},
		{name: "absolute path inside cwd", target: insideFile, want: true},
		{name: "relative path outside cwd", target: "../outside.txt", want: false},
		{name: "file below symlinked directory", target: filepath.Join(directoryLink, "escaped.txt"), want: false},
		{name: "symlink followed by parent directory", target: dotDotEscape, want: false},
		{name: "not yet existing nested path", target: filepath.Join("a", "b", "c", "new.txt"), want: true},
		{name: "new file below inside symlinked directory", target: filepath.Join(insideDirectoryLink, "newfile.txt"), want: true},
		{name: "existing symlink target", target: fileLink, want: false},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Parallel()
			got, reason := pathInside(cwd, tt.target)
			if got != tt.want {
				t.Fatalf("pathInside(%q, %q) = (%v, %q), want %v", cwd, tt.target, got, reason, tt.want)
			}
			if got && reason != "" {
				t.Fatalf("pathInside(%q, %q) returned a reason for an allowed path: %q", cwd, tt.target, reason)
			}
			if !got && reason == "" {
				t.Fatalf("pathInside(%q, %q) returned no reason for a rejected path", cwd, tt.target)
			}
		})
	}
}

func TestEvaluate(t *testing.T) {
	t.Parallel()

	cwd := t.TempDir()
	inside := filepath.Join(cwd, "inside.txt")
	outside := filepath.Join(t.TempDir(), "outside.txt")

	tests := []struct {
		name         string
		sandbox      Sandbox
		approval     ApprovalPolicy
		request      Request
		wantDecision Decision
	}{
		{
			name:         "read-only on-request read file",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("on-request"),
			request:      Request{Tool: "read_file", Path: inside, Cwd: cwd},
			wantDecision: Allow,
		},
		{
			name:         "read-only on-request whitelisted shell",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("on-request"),
			request:      Request{Tool: "shell", Command: "ls", Cwd: cwd},
			wantDecision: Allow,
		},
		{
			name:         "read-only on-request non-whitelisted shell",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("on-request"),
			request:      Request{Tool: "shell", Command: "rm x", Cwd: cwd},
			wantDecision: AskApproval,
		},
		{
			name:         "read-only on-request write inside cwd",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("on-request"),
			request:      Request{Tool: "write_file", Path: inside, Cwd: cwd},
			wantDecision: AskApproval,
		},
		{
			name:         "read-only never non-whitelisted shell",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("never"),
			request:      Request{Tool: "shell", Command: "rm x", Cwd: cwd},
			wantDecision: Deny,
		},
		{
			name:         "read-only never write file",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("never"),
			request:      Request{Tool: "write_file", Path: inside, Cwd: cwd},
			wantDecision: Deny,
		},
		{
			name:         "read-only untrusted whitelisted shell",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("untrusted"),
			request:      Request{Tool: "shell", Command: "ls", Cwd: cwd},
			wantDecision: Allow,
		},
		{
			name:         "read-only untrusted non-whitelisted shell",
			sandbox:      Sandbox("read-only"),
			approval:     ApprovalPolicy("untrusted"),
			request:      Request{Tool: "shell", Command: "rm x", Cwd: cwd},
			wantDecision: AskApproval,
		},
		{
			name:         "workspace-write on-request arbitrary shell",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("on-request"),
			request:      Request{Tool: "shell", Command: "rm x", Cwd: cwd},
			wantDecision: Allow,
		},
		{
			name:         "workspace-write on-request write inside cwd",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("on-request"),
			request:      Request{Tool: "write_file", Path: inside, Cwd: cwd},
			wantDecision: Allow,
		},
		{
			name:         "workspace-write on-request write outside cwd",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("on-request"),
			request:      Request{Tool: "write_file", Path: outside, Cwd: cwd},
			wantDecision: AskApproval,
		},
		{
			name:         "workspace-write never write outside cwd",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("never"),
			request:      Request{Tool: "write_file", Path: outside, Cwd: cwd},
			wantDecision: Deny,
		},
		{
			name:         "workspace-write untrusted write inside cwd",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("untrusted"),
			request:      Request{Tool: "write_file", Path: inside, Cwd: cwd},
			wantDecision: AskApproval,
		},
		{
			name:         "workspace-write untrusted read file",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("untrusted"),
			request:      Request{Tool: "read_file", Path: outside, Cwd: cwd},
			wantDecision: Allow,
		},
		{
			name:         "workspace-write on-failure write outside cwd",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("on-failure"),
			request:      Request{Tool: "write_file", Path: outside, Cwd: cwd},
			wantDecision: AskApproval,
		},
		{
			name:         "danger-full-access never arbitrary shell",
			sandbox:      Sandbox("danger-full-access"),
			approval:     ApprovalPolicy("never"),
			request:      Request{Tool: "shell", Command: "rm -rf /tmp/x", Cwd: cwd},
			wantDecision: Allow,
		},
		{
			name:         "danger-full-access untrusted non-whitelisted shell",
			sandbox:      Sandbox("danger-full-access"),
			approval:     ApprovalPolicy("untrusted"),
			request:      Request{Tool: "shell", Command: "rm x", Cwd: cwd},
			wantDecision: AskApproval,
		},
		{
			name:         "unknown approval policy fails closed",
			sandbox:      Sandbox("workspace-write"),
			approval:     ApprovalPolicy("bogus"),
			request:      Request{Tool: "read_file", Path: inside, Cwd: cwd},
			wantDecision: Deny,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Parallel()
			gotDecision, reason := Evaluate(tt.sandbox, tt.approval, tt.request)
			if gotDecision != tt.wantDecision {
				t.Fatalf("Evaluate(%q, %q, %+v) = (%v, %q), want decision %v", tt.sandbox, tt.approval, tt.request, gotDecision, reason, tt.wantDecision)
			}
			if gotDecision == Allow && reason != "" {
				t.Fatalf("Evaluate(%q, %q, %+v) returned reason %q for an allowed request", tt.sandbox, tt.approval, tt.request, reason)
			}
			if gotDecision != Allow && reason == "" {
				t.Fatalf("Evaluate(%q, %q, %+v) returned no reason for decision %v", tt.sandbox, tt.approval, tt.request, gotDecision)
			}
		})
	}
}

func TestParseSandbox(t *testing.T) {
	t.Parallel()

	for _, value := range []string{"read-only", "workspace-write", "danger-full-access"} {
		value := value
		t.Run(value, func(t *testing.T) {
			t.Parallel()
			got, err := ParseSandbox(value)
			if err != nil {
				t.Fatalf("ParseSandbox(%q) returned error: %v", value, err)
			}
			if got != Sandbox(value) {
				t.Fatalf("ParseSandbox(%q) = %q, want %q", value, got, value)
			}
		})
	}

	if _, err := ParseSandbox("bogus"); err == nil {
		t.Fatal("ParseSandbox(\"bogus\") returned no error")
	}
}

func TestParseApprovalPolicy(t *testing.T) {
	t.Parallel()

	for _, value := range []string{"untrusted", "on-request", "on-failure", "never"} {
		value := value
		t.Run(value, func(t *testing.T) {
			t.Parallel()
			got, err := ParseApprovalPolicy(value)
			if err != nil {
				t.Fatalf("ParseApprovalPolicy(%q) returned error: %v", value, err)
			}
			if got != ApprovalPolicy(value) {
				t.Fatalf("ParseApprovalPolicy(%q) = %q, want %q", value, got, value)
			}
		})
	}

	if _, err := ParseApprovalPolicy("bogus"); err == nil {
		t.Fatal("ParseApprovalPolicy(\"bogus\") returned no error")
	}
}
