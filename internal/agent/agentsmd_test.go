package agent

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func writeAgentsFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func evalDir(t *testing.T, path string) string {
	t.Helper()
	out, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	return out
}

func TestLoadAgentsMDRootToCwd(t *testing.T) {
	root := evalDir(t, t.TempDir())
	writeAgentsFile(t, filepath.Join(root, ".git", "HEAD"), "ref: refs/heads/main\n")
	writeAgentsFile(t, filepath.Join(root, "AGENTS.md"), "root rules")
	writeAgentsFile(t, filepath.Join(root, "svc", "AGENTS.md"), "svc rules")
	cwd := filepath.Join(root, "svc", "pkg")
	if err := os.MkdirAll(cwd, 0o755); err != nil {
		t.Fatal(err)
	}

	got, err := LoadAgentsMD(cwd)
	if err != nil {
		t.Fatalf("LoadAgentsMD() error = %v", err)
	}
	want := "<agents_md path=\"" + filepath.Join(root, "AGENTS.md") + "\">\nroot rules\n</agents_md>\n\n" +
		"<agents_md path=\"" + filepath.Join(root, "svc", "AGENTS.md") + "\">\nsvc rules\n</agents_md>"
	if got != want {
		t.Fatalf("LoadAgentsMD() =\n%s\nwant\n%s", got, want)
	}
}

func TestLoadAgentsMDOutsideRepoReadsOnlyCwd(t *testing.T) {
	parent := evalDir(t, t.TempDir())
	if testutil.GitAncestor(parent) {
		t.Skip("the temp dir sits inside a git checkout, so cwd is not outside a repo")
	}
	writeAgentsFile(t, filepath.Join(parent, "AGENTS.md"), "parent rules")
	cwd := filepath.Join(parent, "child")
	writeAgentsFile(t, filepath.Join(cwd, "AGENTS.md"), "child rules")

	got, err := LoadAgentsMD(cwd)
	if err != nil {
		t.Fatalf("LoadAgentsMD() error = %v", err)
	}
	if strings.Contains(got, "parent rules") || !strings.Contains(got, "child rules") {
		t.Fatalf("LoadAgentsMD() = %q, want only child rules", got)
	}
}

func TestLoadAgentsMDNone(t *testing.T) {
	got, err := LoadAgentsMD(t.TempDir())
	if err != nil || got != "" {
		t.Fatalf("LoadAgentsMD() = (%q, %v), want empty", got, err)
	}
}

func TestLoadAgentsMDTruncatesOnRuneBoundary(t *testing.T) {
	cwd := evalDir(t, t.TempDir())
	// 3-byte runes; 32768 is not a multiple of 3, so a naive byte cut splits a rune.
	writeAgentsFile(t, filepath.Join(cwd, "AGENTS.md"), strings.Repeat("中", 20000))

	got, err := LoadAgentsMD(cwd)
	if err != nil {
		t.Fatalf("LoadAgentsMD() error = %v", err)
	}
	if !utf8.ValidString(got) {
		t.Fatalf("LoadAgentsMD() returned invalid UTF-8")
	}
	if !strings.Contains(got, "[AGENTS.md content truncated at 32768 bytes]") {
		t.Fatalf("LoadAgentsMD() lacks truncation note")
	}
	if n := strings.Count(got, "中"); n*3 > agentsMDMaxBytes {
		t.Fatalf("kept %d bytes of content, want <= %d", n*3, agentsMDMaxBytes)
	}
}

func TestLoadAgentsMDTruncationKeepsContentAfterInvalidByte(t *testing.T) {
	cwd := evalDir(t, t.TempDir())
	// A Latin-1 byte early in an oversized file must not shrink the kept content.
	writeAgentsFile(t, filepath.Join(cwd, "AGENTS.md"), "rule-\xe9-"+strings.Repeat("x", 40000))

	got, err := LoadAgentsMD(cwd)
	if err != nil {
		t.Fatalf("LoadAgentsMD() error = %v", err)
	}
	if n := strings.Count(got, "x"); n < agentsMDMaxBytes-16 {
		t.Fatalf("kept %d content bytes, want about %d", n, agentsMDMaxBytes)
	}
}

func TestLoadAgentsMDReadErrorFails(t *testing.T) {
	cwd := evalDir(t, t.TempDir())
	if err := os.Mkdir(filepath.Join(cwd, "AGENTS.md"), 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadAgentsMD(cwd); err == nil || !strings.Contains(err.Error(), "AGENTS.md") {
		t.Fatalf("LoadAgentsMD() error = %v, want error naming AGENTS.md", err)
	}
}
