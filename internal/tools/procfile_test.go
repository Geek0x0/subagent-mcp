package tools

import (
	"context"
	"os"
	"strconv"
	"strings"
	"testing"
)

// read_file runs inside the server, which can always read its own /proc entries;
// a scrubbed provider key still sits in its environ, so they must be refused
// under every spelling of the path.
func TestReadFileRefusesOwnProcEntries(t *testing.T) {
	pid := strconv.Itoa(os.Getpid())
	for _, path := range []string{
		"/proc/self/environ",
		"/proc/thread-self/environ",
		"/proc/" + pid + "/environ",
		"/proc/" + pid + "/cmdline",
		"/proc/" + pid + "/task/" + pid + "/environ",
		"/tmp/../proc/self/environ",
	} {
		out, err := ReadFile(context.Background(), "/", path)
		if err == nil || !strings.Contains(err.Error(), "own /proc") {
			t.Errorf("ReadFile(%q) = (%q, %v), want the own-/proc refusal", path, out, err)
		}
	}
}

func TestReadFileStillReadsOtherProcEntries(t *testing.T) {
	out, err := ReadFile(context.Background(), "/", "/proc/self/../version")
	if err != nil || out == "" {
		t.Fatalf("ReadFile(/proc/version) = (%q, %v), want it readable", out, err)
	}
}
