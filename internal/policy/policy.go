package policy

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
)

// Sandbox controls which operations can run without leaving the configured sandbox.
type Sandbox string

// ApprovalPolicy controls when operations outside the sandbox require approval.
type ApprovalPolicy string

// Decision is the action the caller should take for a request.
type Decision int

const (
	Allow Decision = iota
	AskApproval
	Deny
)

const (
	sandboxReadOnly         Sandbox = "read-only"
	sandboxWorkspaceWrite   Sandbox = "workspace-write"
	sandboxDangerFullAccess Sandbox = "danger-full-access"
)

const (
	approvalUntrusted ApprovalPolicy = "untrusted"
	approvalOnRequest ApprovalPolicy = "on-request"
	approvalOnFailure ApprovalPolicy = "on-failure"
	approvalNever     ApprovalPolicy = "never"
)

// Request describes an operation to evaluate.
type Request struct {
	Tool     string
	Command  string
	Path     string
	Cwd      string
	BoundCwd *sandbox.Directory
}

// expansionAllowed lists the allowlisted commands that may take unquoted glob,
// brace and tilde words. They only read the files the expansion names and have
// no option that runs a program or writes; find, rg, grep and git do, so a
// file named like one of their options must never be produced by a glob.
var expansionAllowed = map[string]bool{
	"ls": true, "cat": true, "head": true, "tail": true, "wc": true,
	"stat": true, "which": true, "echo": true, "pwd": true,
}

var simpleAllowed = map[string]bool{
	"ls":    true,
	"cat":   true,
	"head":  true,
	"tail":  true,
	"rg":    true,
	"grep":  true,
	"find":  true,
	"pwd":   true,
	"wc":    true,
	"stat":  true,
	"which": true,
	"echo":  true,
}

var gitAllowed = map[string]bool{
	"status":    true,
	"diff":      true,
	"log":       true,
	"show":      true,
	"branch":    true,
	"blame":     true,
	"rev-parse": true,
	"ls-files":  true,
}

// ParseSandbox validates a sandbox value.
func ParseSandbox(s string) (Sandbox, error) {
	sandbox := Sandbox(s)
	switch sandbox {
	case sandboxReadOnly, sandboxWorkspaceWrite, sandboxDangerFullAccess:
		return sandbox, nil
	default:
		return "", fmt.Errorf("invalid sandbox %q", s)
	}
}

// ParseApprovalPolicy validates an approval policy value.
func ParseApprovalPolicy(s string) (ApprovalPolicy, error) {
	policy := ApprovalPolicy(s)
	switch policy {
	case approvalUntrusted, approvalOnRequest, approvalOnFailure, approvalNever:
		return policy, nil
	default:
		return "", fmt.Errorf("invalid approval policy %q", s)
	}
}

// Evaluate combines sandbox and approval policy rules for a request.
func Evaluate(sb Sandbox, ap ApprovalPolicy, req Request) (Decision, string) {
	trusted := req.Tool == "read_file" || (req.Tool == "shell" && isWhitelistedShell(req.Command))
	inSandbox, reason := withinSandbox(sb, req)

	switch ap {
	case approvalUntrusted:
		if trusted {
			return Allow, ""
		}
		if reason == "" {
			reason = "untrusted policy requires approval for non-whitelisted operations"
		}
		return AskApproval, reason
	case approvalNever:
		if inSandbox {
			return Allow, ""
		}
		return Deny, reason
	case approvalOnRequest, approvalOnFailure:
		if inSandbox {
			return Allow, ""
		}
		return AskApproval, reason
	default:
		return Deny, fmt.Sprintf("unknown approval policy %q", ap)
	}
}

func isWhitelistedShell(command string) bool {
	segments, ok := lexShell(command)
	if !ok {
		return false
	}
	for _, segment := range segments {
		fields := segment.words
		if segment.expands && !expansionAllowed[fields[0]] {
			return false
		}
		if simpleAllowed[fields[0]] {
			if hasDangerousShellFlag(fields) {
				return false
			}
			continue
		}
		if fields[0] == "git" && len(fields) > 1 && gitAllowed[fields[1]] {
			if hasDangerousShellFlag(fields) {
				return false
			}
			continue
		}
		return false
	}

	return true
}

func hasDangerousShellFlag(fields []string) bool {
	// ponytail: A command-name and dangerous-flag denylist is not an exhaustive command-line parser; use per-command flag allowlists or OS-level sandboxing to remove this ceiling.
	switch fields[0] {
	case "find":
		denied := map[string]bool{
			"-exec":    true,
			"-execdir": true,
			"-ok":      true,
			"-okdir":   true,
			"-delete":  true,
			"-fprintf": true,
			"-fls":     true,
			"-fprint":  true,
			"-fprint0": true,
		}
		for _, argument := range fields[1:] {
			if denied[argument] {
				return true
			}
		}
	case "rg", "grep":
		for _, argument := range fields[1:] {
			if argument == "--pre" || argument == "--pre-glob" || argument == "--hostname-bin" ||
				strings.HasPrefix(argument, "--pre=") || strings.HasPrefix(argument, "--pre-glob=") ||
				strings.HasPrefix(argument, "--hostname-bin=") {
				return true
			}
		}
	case "git":
		if len(fields) < 2 {
			return false
		}
		for _, argument := range fields[2:] {
			if argument == "--recurse-submodules" || argument == "--show-signature" || argument == "--verify-signatures" ||
				argument == "--submodule" || strings.HasPrefix(argument, "--submodule=") {
				return true
			}
		}
		if fields[1] == "branch" {
			allowed := map[string]bool{
				"-a":             true,
				"--all":          true,
				"-r":             true,
				"--remotes":      true,
				"-v":             true,
				"-vv":            true,
				"--verbose":      true,
				"-l":             true,
				"--list":         true,
				"--show-current": true,
				"--contains":     true,
				"--merged":       true,
				"--no-merged":    true,
				"--sort":         true,
				"--format":       true,
				"--color":        true,
				"--no-color":     true,
			}
			// ponytail: Positional values for --contains, --merged, --no-merged, --sort, and --format are conservatively rejected to prevent branch-name mutations; use an equals form where applicable.
			for _, argument := range fields[2:] {
				if !allowed[argument] && !strings.HasPrefix(argument, "--sort=") && !strings.HasPrefix(argument, "--format=") {
					return true
				}
			}
			return false
		}
		for _, argument := range fields[2:] {
			// %G placeholders make log/show run the configured gpg program.
			if strings.Contains(argument, "%G") {
				return true
			}
		}
		if fields[1] == "blame" {
			for _, argument := range fields[2:] {
				if strings.HasPrefix(argument, "--ext-diff") || strings.HasPrefix(argument, "--textconv") {
					return true
				}
			}
		}
		if fields[1] == "diff" || fields[1] == "log" || fields[1] == "show" {
			for _, argument := range fields[2:] {
				if argument == "--output" || strings.HasPrefix(argument, "--output=") ||
					argument == "-o" || strings.HasPrefix(argument, "-o") ||
					strings.HasPrefix(argument, "--ext-diff") || strings.HasPrefix(argument, "--textconv") {
					return true
				}
			}
		}
	}

	return false
}

func pathInside(cwd, target string, bound ...*sandbox.Directory) (bool, string) {
	_, reason := insideRelative(cwd, target, bound...)
	return reason == "", reason
}

// BoundPath resolves a checked target beneath the session's held cwd inode.
func BoundPath(cwd, target string, bound *sandbox.Directory) (string, error) {
	if bound == nil {
		return "", errors.New("working directory is not bound")
	}
	relative, reason := insideRelative(cwd, target, bound)
	if reason != "" {
		return "", errors.New(reason)
	}
	return filepath.Join(bound.Path(), relative), nil
}

func insideRelative(cwd, target string, bound ...*sandbox.Directory) (string, string) {
	for _, component := range strings.Split(filepath.ToSlash(target), "/") {
		if component == ".." {
			// ponytail: Rejecting every parent-directory component is conservative; resolving each component with Lstat would permit legitimate uses without reopening symlink escapes.
			return "", fmt.Sprintf("target %q contains a parent-directory component", target)
		}
	}

	cwdAbsolute, err := filepath.Abs(cwd)
	if err != nil {
		return "", fmt.Sprintf("resolve working directory %q: %v", cwd, err)
	}
	var resolvedCwd string
	if len(bound) > 0 && bound[0] != nil {
		if err := bound[0].Check(); err != nil {
			return "", err.Error()
		}
		resolvedCwd = bound[0].ResolvedPath()
	} else {
		resolvedCwd, err = filepath.EvalSymlinks(cwdAbsolute)
		if err != nil {
			return "", fmt.Sprintf("resolve working directory %q: %v", cwd, err)
		}
	}

	if !filepath.IsAbs(target) {
		target = filepath.Join(cwdAbsolute, target)
	}
	target, err = filepath.Abs(filepath.Clean(target))
	if err != nil {
		return "", fmt.Sprintf("resolve target path %q: %v", target, err)
	}

	ancestor := target
	var missingParts []string
	for {
		_, statErr := os.Lstat(ancestor)
		if statErr == nil {
			break
		}
		if !os.IsNotExist(statErr) {
			return "", fmt.Sprintf("inspect target ancestor %q: %v", ancestor, statErr)
		}

		parent := filepath.Dir(ancestor)
		if parent == ancestor {
			return "", fmt.Sprintf("no existing ancestor for target %q", target)
		}
		missingParts = append(missingParts, filepath.Base(ancestor))
		ancestor = parent
	}

	resolvedAncestor, err := filepath.EvalSymlinks(ancestor)
	if err != nil {
		return "", fmt.Sprintf("resolve target ancestor %q: %v", ancestor, err)
	}
	resolvedTarget := resolvedAncestor
	for i := len(missingParts) - 1; i >= 0; i-- {
		resolvedTarget = filepath.Join(resolvedTarget, missingParts[i])
	}

	relative, err := filepath.Rel(resolvedCwd, resolvedTarget)
	if err != nil {
		return "", fmt.Sprintf("compare target %q with working directory %q: %v", resolvedTarget, resolvedCwd, err)
	}
	if relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		return "", fmt.Sprintf("target %q resolves outside working directory %q", target, resolvedCwd)
	}
	if len(bound) > 0 && bound[0] != nil {
		if err := bound[0].Check(); err != nil {
			return "", err.Error()
		}
	}
	return relative, ""
}

func withinSandbox(sb Sandbox, req Request) (bool, string) {
	switch sb {
	case sandboxReadOnly:
		if req.Tool == "read_file" || (req.Tool == "shell" && isWhitelistedShell(req.Command)) {
			return true, ""
		}
		return false, "operation is not permitted by the read-only sandbox"
	case sandboxWorkspaceWrite:
		switch req.Tool {
		case "read_file", "shell":
			return true, ""
		case "write_file":
			return pathInside(req.Cwd, req.Path, req.BoundCwd)
		default:
			return false, fmt.Sprintf("tool %q is not permitted by the workspace-write sandbox", req.Tool)
		}
	case sandboxDangerFullAccess:
		return true, ""
	default:
		return false, fmt.Sprintf("unknown sandbox %q", sb)
	}
}
