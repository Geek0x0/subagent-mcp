package provider

import (
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const streamBody = "data: {\"choices\":[{\"delta\":{\"reasoning\":\"hidden\"}}]}\n\ndata: [DONE]\n\n"

func debugServer(t *testing.T) *httptest.Server {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("X-Request-Id", "req-123")
		w.Header().Set("Set-Cookie", "session=secret")
		io.WriteString(w, streamBody)
	}))
	t.Cleanup(server.Close)
	return server
}

func get(t *testing.T, client *http.Client, url string) {
	t.Helper()
	req, err := http.NewRequest("POST", url+"/chat/completions?key=secret-in-query", strings.NewReader(`{"prompt":"private prompt"}`))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer sk-secret-key")
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	if body, _ := io.ReadAll(resp.Body); string(body) != streamBody {
		t.Fatalf("caller read %q, want the body unchanged", body)
	}
	resp.Body.Close()
}

// With the debug directory set, the raw body (including fields the adapters
// ignore) is kept, while keys, prompts, query strings and cookies are not.
func TestDebugStreamRecordsTheRawResponseOnly(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "streams")
	t.Setenv(DebugStreamEnv, dir)
	server := debugServer(t)
	get(t, PooledClient("chat-completions", server.URL, 0), server.URL)

	files, err := filepath.Glob(filepath.Join(dir, "*-chat-completions-*.sse"))
	if err != nil || len(files) != 1 {
		t.Fatalf("recorded files = %v (%v), want exactly one", files, err)
	}
	data, err := os.ReadFile(files[0])
	if err != nil {
		t.Fatal(err)
	}
	text := string(data)
	for _, want := range []string{"# POST http://", "/chat/completions\n", "# status 200", "# Content-Type: text/event-stream", "# X-Request-Id: req-123", streamBody} {
		if !strings.Contains(text, want) {
			t.Errorf("recording lacks %q:\n%s", want, text)
		}
	}
	for _, leak := range []string{"sk-secret-key", "private prompt", "secret-in-query", "session=secret", "Authorization"} {
		if strings.Contains(text, leak) {
			t.Errorf("recording leaks %q:\n%s", leak, text)
		}
	}
	if info, _ := os.Stat(files[0]); info.Mode().Perm() != 0o600 {
		t.Errorf("file mode = %v, want 0600", info.Mode().Perm())
	}
	if info, _ := os.Stat(dir); info.Mode().Perm() != 0o700 {
		t.Errorf("directory mode = %v, want 0700", info.Mode().Perm())
	}
}

func TestDebugStreamIsOffByDefault(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "streams")
	t.Setenv(DebugStreamEnv, "")
	server := debugServer(t)
	get(t, PooledClient("chat-completions", server.URL, 0), server.URL)
	if _, err := os.Stat(dir); !os.IsNotExist(err) {
		t.Fatalf("debug directory exists without the variable set (err %v)", err)
	}
}

// A directory that cannot be created must not break the request.
func TestDebugStreamFailureDoesNotBreakRequests(t *testing.T) {
	blocker := filepath.Join(t.TempDir(), "file")
	if err := os.WriteFile(blocker, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv(DebugStreamEnv, filepath.Join(blocker, "sub"))
	server := debugServer(t)
	get(t, PooledClient("chat-completions", server.URL, 0), server.URL)
}
