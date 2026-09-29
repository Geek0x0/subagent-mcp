package chatcompletions

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"slices"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

// The credential environment a Claude Code installation configured for a
// gateway may export must never reach a request this adapter sends to the
// configured base_url: the wire may carry only the configured key in
// Authorization plus the SDK's own protocol/content headers. go-openai is
// not known to read the environment; this test proves it rather than
// assuming it, with the same hostile environment the other adapters are
// tested with.

const (
	configuredKey = "cfg-key-0123"
	envAPIKey     = "sk-ant-env-DIFFERENT-key"
	envAuthToken  = "sk-ant-oat-SECRET-from-env"
	envGatewayHdr = "gw-secret" // X-Corp-Gateway-Token value in ANTHROPIC_CUSTOM_HEADERS
	envOpenAIKey  = "sk-openai-env-DIFFERENT-key"
	envAdminKey   = "sk-admin-env-SECRET"
	envOrgID      = "org-real"
	envProjectID  = "proj-real"
	envOpenAIGw   = "gw-SECRET" // X-Corp-Gateway-Token value in OPENAI_CUSTOM_HEADERS
)

// credentialEnvVars are the environment variables that must not influence
// what this adapter puts on the wire.
var credentialEnvVars = []string{
	"ANTHROPIC_API_KEY",
	"ANTHROPIC_AUTH_TOKEN",
	"ANTHROPIC_CUSTOM_HEADERS",
	"ANTHROPIC_BASE_URL",
	"OPENAI_API_KEY",
	"OPENAI_ADMIN_KEY",
	"OPENAI_ORG_ID",
	"OPENAI_PROJECT_ID",
	"OPENAI_CUSTOM_HEADERS",
	"OPENAI_BASE_URL",
}

// hostileEnv is the full credential environment under test; the base URLs
// point at capture servers that must receive no request at all.
func hostileEnv(anthropicElsewhere, openaiElsewhere string) map[string]string {
	return map[string]string{
		"ANTHROPIC_API_KEY":        envAPIKey,
		"ANTHROPIC_AUTH_TOKEN":     envAuthToken,
		"ANTHROPIC_CUSTOM_HEADERS": "X-Corp-Gateway-Token: " + envGatewayHdr,
		"ANTHROPIC_BASE_URL":       anthropicElsewhere,
		"OPENAI_API_KEY":           envOpenAIKey,
		"OPENAI_ADMIN_KEY":         envAdminKey,
		"OPENAI_ORG_ID":            envOrgID,
		"OPENAI_PROJECT_ID":        envProjectID,
		"OPENAI_CUSTOM_HEADERS":    "X-Corp-Gateway-Token: " + envOpenAIGw,
		"OPENAI_BASE_URL":          openaiElsewhere,
	}
}

// forbiddenEnvValues must not appear inside any outbound header value.
func forbiddenEnvValues(anthropicElsewhere, openaiElsewhere string) []string {
	return []string{
		envAPIKey, envAuthToken, envGatewayHdr,
		envOpenAIKey, envAdminKey, envOrgID, envProjectID, envOpenAIGw,
		anthropicElsewhere, openaiElsewhere,
	}
}

// forbiddenEnvHeaders must not appear as header names at all; their values
// are environment-derived by construction.
var forbiddenEnvHeaders = []string{
	"X-Corp-Gateway-Token", // from *_CUSTOM_HEADERS
	"OpenAI-Organization",  // from OPENAI_ORG_ID
	"OpenAI-Project",       // from OPENAI_PROJECT_ID
}

// newElsewhereServer starts a capture server used as an environment
// ANTHROPIC_BASE_URL/OPENAI_BASE_URL target; it must stay unused.
func newElsewhereServer(t *testing.T) (string, *atomic.Int64) {
	t.Helper()
	var hits atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.Header().Set("Content-Type", "application/json")
		_, _ = io.WriteString(w, `{"data":[]}`)
	}))
	t.Cleanup(srv.Close)
	return srv.URL, &hits
}

// clearCredentialEnv removes every credential environment variable so the
// positive control runs in a plain environment regardless of the machine.
func clearCredentialEnv(t *testing.T) {
	t.Helper()
	for _, key := range credentialEnvVars {
		original, existed := os.LookupEnv(key)
		if err := os.Unsetenv(key); err != nil {
			t.Fatalf("unset %s: %v", key, err)
		}
		t.Cleanup(func() {
			if existed {
				_ = os.Setenv(key, original)
			}
		})
	}
}

func assertNoEnvironmentHeaders(t *testing.T, h http.Header, op string, forbiddenValues []string) {
	t.Helper()
	for name, values := range h {
		canonical := http.CanonicalHeaderKey(name)
		for _, forbidden := range forbiddenEnvHeaders {
			if canonical == http.CanonicalHeaderKey(forbidden) {
				t.Errorf("%s: environment-derived header %s: %v", op, name, values)
			}
		}
		for _, value := range values {
			for _, secret := range forbiddenValues {
				if strings.Contains(value, secret) {
					t.Errorf("%s: environment value %q leaked into header %s: %v", op, secret, name, values)
				}
			}
		}
	}
}

func assertChatWireCredentials(t *testing.T, h http.Header, op string, forbiddenValues []string) {
	t.Helper()
	if got := h.Values("Authorization"); !slices.Equal(got, []string{"Bearer " + configuredKey}) {
		t.Errorf("%s: Authorization = %v, want [Bearer %s]", op, got, configuredKey)
	}
	if h.Get("Accept") == "" {
		t.Errorf("%s: standard Accept header missing", op)
	}
	assertNoEnvironmentHeaders(t, h, op, forbiddenValues)
}

// TestChatCompletionsEnvCredentialsNeverReachTheWire runs a normal turn and
// ListModels against a capture server used as base_url with the whole
// credential environment hostile, and asserts the configured server saw only
// the configured key plus standard protocol headers. The plain-environment
// case is the positive control proving the requests really succeed.
func TestChatCompletionsEnvCredentialsNeverReachTheWire(t *testing.T) {
	anthropicElsewhere, anthropicHits := newElsewhereServer(t)
	openaiElsewhere, openaiHits := newElsewhereServer(t)
	forbiddenValues := forbiddenEnvValues(anthropicElsewhere, openaiElsewhere)

	exercise := func(t *testing.T, hitsBefore int64) {
		t.Helper()
		fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Text: "hello"}})
		fake.SetModels([]string{"model-a"})
		p, err := New("deepseek", config.Provider{
			API: config.APIChatCompletions, BaseURL: fake.URL,
		}, configuredKey)
		if err != nil {
			t.Fatal(err)
		}

		res, err := p.Turn(context.Background(), provider.TurnRequest{
			Model:    "m",
			Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
		}, nil)
		if err != nil {
			t.Fatalf("Turn: %v", err)
		}
		if res.Text != "hello" {
			t.Fatalf("Turn text = %q, want hello", res.Text)
		}
		ids, err := p.(provider.ModelLister).ListModels(context.Background())
		if err != nil {
			t.Fatalf("ListModels: %v", err)
		}
		if !slices.Equal(ids, []string{"model-a"}) {
			t.Fatalf("ListModels = %v, want [model-a]", ids)
		}
		if fake.RequestHeaderCount() != 2 {
			t.Fatalf("configured server recorded %d requests, want exactly 2 (turn, ListModels)", fake.RequestHeaderCount())
		}
		if ct := fake.RequestHeaders(0).Get("Content-Type"); !strings.HasPrefix(ct, "application/json") {
			t.Errorf("turn Content-Type = %q, want application/json", ct)
		}
		assertChatWireCredentials(t, fake.RequestHeaders(0), "turn", forbiddenValues)
		assertChatWireCredentials(t, fake.RequestHeaders(1), "ListModels", forbiddenValues)
		if hits := anthropicHits.Load() + openaiHits.Load(); hits != hitsBefore {
			t.Fatalf("%d request(s) went to the environment base_url instead of the configured one", hits-hitsBefore)
		}
	}

	t.Run("hostile credential environment", func(t *testing.T) {
		for key, value := range hostileEnv(anthropicElsewhere, openaiElsewhere) {
			t.Setenv(key, value)
		}
		exercise(t, anthropicHits.Load()+openaiHits.Load())
	})

	t.Run("plain environment positive control", func(t *testing.T) {
		clearCredentialEnv(t)
		exercise(t, anthropicHits.Load()+openaiHits.Load())
	})
}
