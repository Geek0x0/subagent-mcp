package tools

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
)

const MaxOutputBytes = 16 * 1024

const (
	DefaultShellTimeout = 60 * time.Second
	MaxShellTimeout     = 600 * time.Second
)

type limitedBuffer struct {
	buf   bytes.Buffer
	total int
}

func (b *limitedBuffer) Write(p []byte) (int, error) {
	n := len(p)
	b.total += n
	if remaining := MaxOutputBytes - b.buf.Len(); remaining > 0 {
		if len(p) > remaining {
			p = p[:remaining]
		}
		_, _ = b.buf.Write(p)
	}
	return n, nil
}

func (b *limitedBuffer) String() string {
	out := b.buf.String()
	if b.total > MaxOutputBytes {
		out += fmt.Sprintf("\n[output truncated: %d bytes total]", b.total)
	}
	return out
}

func RunShell(ctx context.Context, cwd, command string, timeout time.Duration) (out string, exitCode int, err error) {
	return runCommand(ctx, cwd, shellArgv(command), timeout)
}

// RunShellSandboxed runs command through the Landlock helper so that only
// writableRoots are writable by the command and its descendants.
func RunShellSandboxed(
	ctx context.Context,
	cwd, command string,
	timeout time.Duration,
	writableRoots []string,
) (out string, exitCode int, err error) {
	self, err := os.Executable()
	if err != nil {
		return "", -1, fmt.Errorf("locate sandbox helper: %w", err)
	}
	return runCommand(ctx, cwd, sandbox.Command(self, writableRoots, shellArgv(command)...), timeout)
}

// RunShellSandboxedBound gives the helper session-held directory handles. The
// helper starts in the held cwd and grants writes to the held root inodes.
func RunShellSandboxedBound(
	ctx context.Context,
	command string,
	timeout time.Duration,
	cwd *sandbox.Directory,
	writableRoots []*sandbox.Directory,
) (out string, exitCode int, err error) {
	if cwd == nil {
		return "", -1, errors.New("sandbox working directory is not bound")
	}
	self, err := os.Executable()
	if err != nil {
		return "", -1, fmt.Errorf("locate sandbox helper: %w", err)
	}
	files := []*os.File{cwd.File()}
	rootFDs := make([]int, 0, len(writableRoots))
	for _, root := range writableRoots {
		files = append(files, root.File())
		rootFDs = append(rootFDs, len(files)+2) // ExtraFiles starts at fd 3.
	}
	argv := sandbox.CommandFD(self, 3, rootFDs, shellArgv(command)...)
	return runCommandWithFiles(ctx, "/", argv, timeout, files)
}

// shellScript removes variables after bash has loaded the user's login profile,
// then evaluates the caller's command with the profile's other environment
// changes intact. It also removes profile aliases/functions that could replace
// an allowlisted executable and wraps allowlisted Git reads with safe config
// overrides. The names and prefixes are positional arguments, not shell
// source, so a configured name can never inject code into this wrapper.
const shellScript = `exact_count=$1
prefix_count=$2
shift 2
while ((exact_count > 0)); do
    builtin unset -- "$1"
    shift
    exact_count=$((exact_count - 1))
done
while ((prefix_count > 0)); do
    prefix=$1
    shift
    while IFS= read -r name; do
        if [[ $name == "$prefix"* ]]; then
            builtin unset -- "$name"
        fi
    done < <(builtin compgen -A variable)
    prefix_count=$((prefix_count - 1))
done
builtin shopt -u expand_aliases
builtin unalias -a 2>/dev/null
for __subagent_mcp_name in ls cat head tail rg grep find pwd wc stat which echo git; do
    builtin unset -f "$__subagent_mcp_name" 2>/dev/null
done
builtin unset -- __subagent_mcp_name
__subagent_mcp_safe_git() {
    local subcommand=${1-}
    case "$subcommand" in
    status|branch|rev-parse|ls-files|diff|show|log|blame) ;;
    *) command git "$@"; return ;;
    esac
    shift
    # Overrides travel in GIT_CONFIG_KEY_n/VALUE_n (Git 2.31+), which take a key
    # verbatim: "-c key=value" splits at the first "=", so a filter or diff driver
    # whose name contains "=" would slip through. Probe support first; without
    # it the overrides would be silently ignored.
    if [[ $(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=subagent.probe GIT_CONFIG_VALUE_0=ok \
            command git config --get subagent.probe 2>/dev/null) != ok ]]; then
        echo "subagent-mcp: git 2.31 or newer is required to run git safely" >&2
        return 126
    fi
    local -a fixed=(
        "alias.$subcommand=" core.fsmonitor=false core.fsmonitorHookPath= diff.external=
        core.pager=cat core.sshCommand= credential.helper= core.hooksPath=/dev/null
        log.showSignature=false status.submoduleSummary=false submodule.recurse=false
        diff.submodule=short interactive.diffFilter=
        gpg.program=/bin/false gpg.openpgp.program=/bin/false gpg.x509.program=/bin/false
        gpg.ssh.program=/bin/false
    )
    local -a keys=() vals=()
    local item drivers driver_status driver_key
    for item in "${fixed[@]}"; do
        keys+=("${item%%=*}")
        vals+=("${item#*=}")
    done
    # Every program a filter or diff driver could run, whichever config file
    # (repository, user, or included) defines it, is emptied.
    drivers=$(
        GIT_CONFIG_COUNT=0 GIT_CONFIG_PARAMETERS= \
        command git --no-pager -c alias.config= config --name-only --get-regexp \
            '^(filter\..*\.(clean|smudge|process|required)|diff\..*\.(textconv|command))$'
    )
    driver_status=$?
    if ((driver_status > 1)); then
        return "$driver_status"
    fi
    if ((driver_status == 0)); then
        while IFS= read -r driver_key; do
            if [[ -n $driver_key ]]; then
                keys+=("$driver_key")
                case "$driver_key" in
                *.required) vals+=(false) ;;
                *) vals+=("") ;;
                esac
            fi
        done <<< "$drivers"
    fi
    case "$subcommand" in
    diff|show|log|blame) set -- --no-ext-diff --no-textconv "$@" ;;
    esac
    (
        local i
        export GIT_CONFIG_COUNT=${#keys[@]} GIT_CONFIG_PARAMETERS= GIT_OPTIONAL_LOCKS=0 \
            GIT_EXTERNAL_DIFF= GIT_PAGER=cat PAGER=cat GIT_SSH= GIT_SSH_COMMAND= GIT_ASKPASS=
        for ((i = 0; i < ${#keys[@]}; i++)); do
            export "GIT_CONFIG_KEY_$i=${keys[i]}" "GIT_CONFIG_VALUE_$i=${vals[i]}"
        done
        command git --no-pager "$subcommand" "$@"
    )
}
git() {
    __subagent_mcp_safe_git "$@"
}
__subagent_mcp_command=$1
set --
builtin eval "$__subagent_mcp_command"`

func shellArgv(command string) []string {
	scrubMu.RLock()
	names := make([]string, 0, len(scrubNames))
	for name := range scrubNames {
		names = append(names, name)
	}
	sort.Strings(names)
	prefixes := append([]string(nil), scrubPrefixes...)
	scrubMu.RUnlock()

	argv := []string{
		"bash", "-lc", shellScript, "bash",
		strconv.Itoa(len(names)), strconv.Itoa(len(prefixes)),
	}
	argv = append(argv, names...)
	argv = append(argv, prefixes...)
	return append(argv, command)
}

func runCommand(ctx context.Context, cwd string, argv []string, timeout time.Duration) (out string, exitCode int, err error) {
	return runCommandWithFiles(ctx, cwd, argv, timeout, nil)
}

func runCommandWithFiles(ctx context.Context, cwd string, argv []string, timeout time.Duration, files []*os.File) (out string, exitCode int, err error) {
	if timeout <= 0 {
		timeout = DefaultShellTimeout
	} else if timeout > MaxShellTimeout {
		timeout = MaxShellTimeout
	}

	runCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	var buf limitedBuffer
	cmd := exec.CommandContext(runCtx, argv[0], argv[1:]...)
	cmd.Dir = cwd
	cmd.ExtraFiles = files
	cmd.Env = ScrubbedEnv()
	// ponytail: Linux process-group signaling kills ordinary descendants, and WaitDelay bounds
	// inherited-pipe waits. A descendant that escapes the group can survive without being reported.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error {
		return syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
	}
	cmd.WaitDelay = time.Second
	cmd.Stdout = &buf
	cmd.Stderr = &buf

	runErr := cmd.Run()
	out = buf.String()
	if errors.Is(runErr, exec.ErrWaitDelay) {
		return out, 0, nil
	}
	if errors.Is(runCtx.Err(), context.DeadlineExceeded) {
		return out, -1, fmt.Errorf("command timed out after %s", timeout)
	}
	if runCtx.Err() != nil {
		return out, -1, runCtx.Err()
	}
	if runErr == nil {
		return out, 0, nil
	}

	var exitErr *exec.ExitError
	if errors.As(runErr, &exitErr) {
		return out, exitErr.ExitCode(), nil
	}
	return out, -1, runErr
}

var (
	scrubMu       sync.RWMutex
	scrubNames    = map[string]bool{}
	scrubPrefixes = []string{"SUBAGENT_MCP_"}
	protected     []string
)

// SetScrubbedEnv sets which environment variables are removed from every shell
// child: exact names plus every name starting with one of the prefixes.
func SetScrubbedEnv(names, prefixes []string) {
	scrubMu.Lock()
	defer scrubMu.Unlock()
	scrubNames = make(map[string]bool, len(names))
	for _, name := range names {
		scrubNames[name] = true
	}
	scrubPrefixes = append([]string(nil), prefixes...)
}

// SetProtectedFiles sets the files read_file refuses to read, compared after
// resolving symlinks.
func SetProtectedFiles(paths []string) {
	scrubMu.Lock()
	defer scrubMu.Unlock()
	protected = protected[:0]
	for _, path := range paths {
		if absolute, err := filepath.Abs(path); err == nil {
			path = absolute
		}
		protected = append(protected, resolveForComparison(path))
	}
}

// ScrubbedEnv returns the current environment without the configured secret
// names and prefixes so that commands run by the agent cannot read provider
// API keys from the server's environment.
func ScrubbedEnv() []string {
	scrubMu.RLock()
	defer scrubMu.RUnlock()
	env := os.Environ()
	scrubbed := make([]string, 0, len(env))
	for _, entry := range env {
		name, _, _ := strings.Cut(entry, "=")
		if scrubNames[name] {
			continue
		}
		strip := false
		for _, prefix := range scrubPrefixes {
			if strings.HasPrefix(name, prefix) {
				strip = true
				break
			}
		}
		if strip {
			continue
		}
		scrubbed = append(scrubbed, entry)
	}
	return scrubbed
}

// ReadFile reads a file, returning at most MaxOutputBytes bytes. It is
// equivalent to ReadFileRange with a zero offset and no line limit.
func ReadFile(ctx context.Context, cwd, path string) (string, error) {
	return ReadFileRange(ctx, cwd, path, 0, 0)
}

// ReadFileRange reads at most limit lines starting at the 1-based line offset,
// returning at most MaxOutputBytes bytes. A non-positive offset means line 1
// and a non-positive limit means no line limit. Continuation markers in the
// result tell the caller which line to request next.
func ReadFileRange(ctx context.Context, cwd, path string, offset, limit int) (string, error) {
	if isProtectedFile(cwd, path) {
		return "", fmt.Errorf("refusing to read a protected subagent-mcp file: %s", resolvePath(cwd, path))
	}
	if offset < 1 {
		offset = 1
	}

	readCtx, cancel := context.WithTimeout(ctx, DefaultShellTimeout)
	defer cancel()

	type readResult struct {
		content string
		err     error
	}

	resultCh := make(chan readResult, 1)
	fileCh := make(chan *os.File)
	// ponytail: os.Open is not context-aware, so a timeout can leave its goroutine blocked
	// until the open returns. A platform-specific nonblocking open plus polling could make
	// opening fully cancellable.
	go func() {
		file, err := os.Open(resolvePath(cwd, path))
		if err != nil {
			resultCh <- readResult{err: err}
			return
		}
		if isOwnProcessFile(file) {
			_ = file.Close()
			resultCh <- readResult{err: errors.New("refusing to read the subagent-mcp process's own /proc entries")}
			return
		}
		select {
		case fileCh <- file:
		case <-readCtx.Done():
			_ = file.Close()
			return
		}
		defer file.Close()

		content, err := readFileRange(file, offset, limit)
		resultCh <- readResult{content: content, err: err}
	}()

	var file *os.File
	for {
		select {
		case file = <-fileCh:
			fileCh = nil
		case result := <-resultCh:
			if result.err != nil {
				return "", result.err
			}
			return result.content, nil
		case <-readCtx.Done():
			if file != nil {
				_ = file.Close()
			}
			if errors.Is(readCtx.Err(), context.DeadlineExceeded) {
				return "", fmt.Errorf("file read timed out: %w", readCtx.Err())
			}
			return "", readCtx.Err()
		}
	}
}

// readFileRange streams the open file with a bufio.Reader, stopping as soon as
// the output cap or the line limit is reached.
func readFileRange(file *os.File, offset, limit int) (string, error) {
	reader := bufio.NewReaderSize(file, MaxOutputBytes+1)

	var totalSize int64
	if info, statErr := file.Stat(); statErr == nil {
		totalSize = info.Size()
	}
	var bytesRead int64

	lineNo := 1
	newlines := 0
	for lineNo < offset {
		line, err := reader.ReadSlice('\n')
		bytesRead += int64(len(line))
		switch {
		case err == nil:
			newlines++
			lineNo++
		case errors.Is(err, bufio.ErrBufferFull):
			// Still inside a line longer than the reader buffer.
		case errors.Is(err, io.EOF):
			lines := newlines
			if len(line) > 0 {
				lines++
			}
			return fmt.Sprintf("[offset %d is past the end of the file (%d lines)]", offset, lines), nil
		default:
			return "", err
		}
	}

	out := make([]byte, 0, MaxOutputBytes)
	returned := 0
	for {
		if limit > 0 && returned >= limit {
			if _, err := reader.Peek(1); err != nil {
				return string(out), nil
			}
			return string(out) + fmt.Sprintf("\n[more lines follow; continue with offset=%d]", lineNo), nil
		}

		line, err := reader.ReadSlice('\n')
		bytesRead += int64(len(line))
		if err != nil && !errors.Is(err, bufio.ErrBufferFull) && !errors.Is(err, io.EOF) {
			return "", err
		}
		if len(line) == 0 {
			// End of file at a line boundary.
			if len(out) == 0 && offset > 1 {
				return fmt.Sprintf("[offset %d is past the end of the file (%d lines)]", offset, offset-1), nil
			}
			return string(out), nil
		}
		if len(out)+len(line) > MaxOutputBytes {
			// The current line does not fit. Keep whole lines when at least one
			// whole line was returned; when a single line alone exceeds the cap,
			// keep its first MaxOutputBytes bytes and resume after it.
			if len(out) == 0 {
				out = append(out, line[:MaxOutputBytes]...)
				lineNo++
			}
			size := totalSize
			if bytesRead > size {
				size = bytesRead
			}
			return string(out) +
				fmt.Sprintf("\n[content truncated: %d bytes total; continue with offset=%d]", size, lineNo), nil
		}
		out = append(out, line...)
		if err == nil {
			returned++
			lineNo++
		}
		if errors.Is(err, io.EOF) {
			// The final line without a trailing newline was returned in full.
			return string(out), nil
		}
	}
}

// isOwnProcessFile reports whether the opened file is an entry of this
// process's /proc directory (environ, mem, cmdline, fd/*, ...). PR_SET_DUMPABLE
// keeps other processes out, but the server itself can always read them, and
// read_file runs inside the server. The check is made on the opened descriptor,
// so /proc/self, /proc/thread-self and symlinks to them are all seen by their
// real name, with no gap between check and open.
func isOwnProcessFile(file *os.File) bool {
	target, err := os.Readlink("/proc/self/fd/" + strconv.Itoa(int(file.Fd())))
	if err != nil {
		return false
	}
	own := "/proc/" + strconv.Itoa(os.Getpid())
	return target == own || strings.HasPrefix(target, own+"/")
}

func isProtectedFile(cwd, path string) bool {
	resolved := resolveForComparison(resolvePath(cwd, path))
	scrubMu.RLock()
	defer scrubMu.RUnlock()
	for _, candidate := range protected {
		if resolved == candidate {
			return true
		}
	}
	return false
}

// resolveForComparison follows symlinks when possible so that a link pointing
// at a protected file compares equal to the file itself. When resolution fails
// (for example because the path does not exist), it falls back to a lexical
// comparison.
func resolveForComparison(path string) string {
	if resolved, err := filepath.EvalSymlinks(path); err == nil {
		return resolved
	}
	return filepath.Clean(path)
}

func WriteFile(cwd, path, content string) error {
	path = resolvePath(cwd, path)
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	return os.WriteFile(path, []byte(content), 0o644)
}

func resolvePath(cwd, path string) string {
	if filepath.IsAbs(path) {
		return path
	}
	return filepath.Join(cwd, path)
}
