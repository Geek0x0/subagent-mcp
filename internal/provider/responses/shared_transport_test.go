package responses

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

// E1: sessions against one endpoint must reuse one connection pool while each
// session keeps its own credential.

// newSession builds one adapter the way one server session does: its own
// client, its own key, the configured base URL.
func newSession(t *testing.T, name, baseURL, key string) provider.Provider {
	t.Helper()
	p, err := New(name, config.Provider{API: config.APIResponses, BaseURL: baseURL}, key)
	if err != nil {
		t.Fatalf("New(%s): %v", name, err)
	}

	return p
}

// sessionTurn runs one turn; the model identifies the session in the fake's
// recorded request, so the key that arrived can be checked per session.
func sessionTurn(t *testing.T, p provider.Provider, model, wantText string) {
	t.Helper()
	if err := runSessionTurn(p, model, wantText); err != nil {
		t.Fatal(err)
	}
}

// runSessionTurn is sessionTurn without test-goroutine assertions, so stress
// goroutines can report failures with t.Errorf instead of t.Fatal.
func runSessionTurn(p provider.Provider, model, wantText string) error {
	res, err := p.Turn(context.Background(), provider.TurnRequest{
		Model:    model,
		Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
	}, nil)
	if err != nil {
		return fmt.Errorf("Turn(%s): %w", model, err)
	}
	if res.Text != wantText {
		return fmt.Errorf("Turn(%s) text = %q, want %q", model, res.Text, wantText)
	}

	return nil
}

// sessionKey strips the Bearer prefix so tests can compare arrived keys.
func sessionKey(h http.Header) string { return strings.TrimPrefix(h.Get("Authorization"), "Bearer ") }

// TestSequentialSessionsShareOneConnection builds two adapters one after
// another for the same base URL with different keys and runs a turn on each:
// the endpoint must see exactly one TCP connection, and each request must
// carry its own session's key.
func TestSequentialSessionsShareOneConnection(t *testing.T) {
	fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{
		{Items: []testutil.FakeResponseItem{{MessageText: "first"}}},
		{Items: []testutil.FakeResponseItem{{MessageText: "second"}}},
	})

	first := newSession(t, "session-1", fake.URL, "key-first")
	second := newSession(t, "session-2", fake.URL, "key-second")

	sessionTurn(t, first, "model-1", "first")
	sessionTurn(t, second, "model-2", "second")

	if conns := fake.ConnectionCount(); conns != 1 {
		t.Errorf("two sequential sessions opened %d TCP connections to the endpoint, want 1 (each session must reuse the endpoint's pool)", conns)
	}
	if key := sessionKey(fake.RequestHeaders(0)); key != "key-first" {
		t.Errorf("session 1 sent Authorization key = %q, want its own key %q", key, "key-first")
	}
	if key := sessionKey(fake.RequestHeaders(1)); key != "key-second" {
		t.Errorf("session 2 sent Authorization key = %q, want its own key %q", key, "key-second")
	}
}

// TestTransportSharedWithinEndpointScopedToIt proves the pool an adapter uses
// is the one registered for its base URL: the test opens a connection through
// the registered transport first, so an adapter that shares that pool reuses
// the connection while an adapter with a pool of its own opens a second one.
// Two different base URLs must get two different transports.
func TestTransportSharedWithinEndpointScopedToIt(t *testing.T) {
	fakeA := testutil.NewFakeResponses(t, []testutil.FakeResponse{{Items: []testutil.FakeResponseItem{{MessageText: "a"}}}})
	fakeB := testutil.NewFakeResponses(t, []testutil.FakeResponse{{Items: []testutil.FakeResponseItem{{MessageText: "b"}}}})

	transportA := provider.SharedTransport(provider.KindResponses, fakeA.URL, 0)
	transportB := provider.SharedTransport(provider.KindResponses, fakeB.URL, 0)
	if transportA == transportB {
		t.Fatalf("different base URLs %s and %s share one transport", fakeA.URL, fakeB.URL)
	}
	if again := provider.SharedTransport(provider.KindResponses, fakeA.URL, 0); again != transportA {
		t.Fatalf("base URL %s got a different transport on the second lookup", fakeA.URL)
	}

	warmResponsesEndpoint(t, fakeA.URL, transportA)
	warmResponsesEndpoint(t, fakeB.URL, transportB)
	if conns := fakeA.ConnectionCount(); conns != 1 {
		t.Fatalf("warm-up on endpoint A opened %d connections, want 1", conns)
	}

	sessionTurn(t, newSession(t, "session-a", fakeA.URL, "key-a"), "model-a", "a")
	sessionTurn(t, newSession(t, "session-b", fakeB.URL, "key-b"), "model-b", "b")

	if conns := fakeA.ConnectionCount(); conns != 1 {
		t.Errorf("endpoint A accepted %d connections after its own adapter's turn, want 1 (the adapter must use the pool registered for its base URL)", conns)
	}
	if conns := fakeB.ConnectionCount(); conns != 1 {
		t.Errorf("endpoint B accepted %d connections after its own adapter's turn, want 1 (the adapter must use the pool registered for its base URL)", conns)
	}
}

// warmResponsesEndpoint sends one trivially valid request through transport so
// the endpoint has an idle connection in that transport's pool.
func warmResponsesEndpoint(t *testing.T, baseURL string, transport http.RoundTripper) {
	t.Helper()
	resp, err := (&http.Client{Transport: transport}).Get(baseURL + "/models")
	if err != nil {
		t.Fatalf("warm-up request: %v", err)
	}
	_, _ = io.Copy(io.Discard, resp.Body)
	_ = resp.Body.Close()
}

// TestConcurrentSessionsKeepKeysOnSharedPool builds adapters and runs turns
// from many goroutines at once: the shared registry must be race-free and no
// session's key may leak into another session's request.
func TestConcurrentSessionsKeepKeysOnSharedPool(t *testing.T) {
	const sessions = 24 // per endpoint: sessions / 2
	scripted := make([]testutil.FakeResponse, sessions/2)
	for i := range scripted {
		scripted[i] = testutil.FakeResponse{Items: []testutil.FakeResponseItem{{MessageText: "ok"}}}
	}
	fakeA := testutil.NewFakeResponses(t, scripted)
	fakeB := testutil.NewFakeResponses(t, scripted)

	wantKey := make(map[string]string, sessions) // model -> that session's key
	for i := 0; i < sessions; i++ {
		wantKey[fmt.Sprintf("model-%d", i)] = fmt.Sprintf("key-%02d", i)
	}

	var wg sync.WaitGroup
	for i := 0; i < sessions; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			baseURL := fakeA.URL
			if i%2 == 1 {
				baseURL = fakeB.URL
			}
			p, err := New(fmt.Sprintf("session-%d", i), config.Provider{API: config.APIResponses, BaseURL: baseURL}, fmt.Sprintf("key-%02d", i))
			if err != nil {
				t.Errorf("session %d: New: %v", i, err)
				return
			}
			if err := runSessionTurn(p, fmt.Sprintf("model-%d", i), "ok"); err != nil {
				t.Errorf("session %d: %v", i, err)
			}
		}(i)
	}
	wg.Wait()

	for _, fake := range []*testutil.FakeResponses{fakeA, fakeB} {
		if got := fake.RequestHeaderCount(); got != sessions/2 {
			t.Errorf("endpoint %s served %d requests, want %d", fake.URL, got, sessions/2)
			continue
		}
		for j := 0; j < fake.RequestHeaderCount(); j++ {
			model, _ := fake.Request(j)["model"].(string)
			want, ok := wantKey[model]
			if !ok {
				t.Errorf("endpoint %s: request %d has unexpected model %q", fake.URL, j, model)
				continue
			}
			if key := sessionKey(fake.RequestHeaders(j)); key != want {
				t.Errorf("endpoint %s: %s carried Authorization key = %q, want that session's key %q", fake.URL, model, key, want)
			}
		}
	}
}
