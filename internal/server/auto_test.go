package server

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func newAutoStub(t *testing.T, response string, status int) (*httptest.Server, *[]map[string]any) {
	t.Helper()
	var requests []map[string]any
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var request map[string]any
		_ = json.NewDecoder(r.Body).Decode(&request)
		requests = append(requests, request)
		w.WriteHeader(status)
		_, _ = io.WriteString(w, response)
	}))
	t.Cleanup(server.Close)
	return server, &requests
}

func autoResponse(choice string, confidence float64) string {
	return `{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"` + choice + `","confidence":` +
		jsonNumber(confidence) + `,"probabilities":{}}},"usage":{"input_tokens":42,"output_tokens":3}}`
}

func jsonNumber(value float64) string {
	encoded, _ := json.Marshal(value)
	return string(encoded)
}

func autoConfig(t *testing.T, baseURL string) *config.Config {
	t.Helper()
	cfg := testMultiConfig(t, map[string]provider.Provider{
		"alpha": &stubProvider{name: "alpha"},
		"bravo": &stubProvider{name: "bravo"},
	})
	cfg.Auto = &config.Auto{
		BaseURL:        baseURL,
		EnvKey:         "TEST_AUTO_KEY",
		Model:          "jev-latest",
		TimeoutSeconds: 5,
	}
	return cfg
}

func TestResolveAutoSelectsProviderFromJev(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	jev, requests := newAutoStub(t, autoResponse("bravo/m-pro", 0.9), http.StatusOK)
	cfg := autoConfig(t, jev.URL)
	s := New(cfg, "test")

	sel, errResult := s.resolveAuto(context.Background(), "fix the parser", t.TempDir(), "")
	if errResult != nil {
		t.Fatalf("resolveAuto() = %s", toolResultText(t, errResult))
	}
	if sel.providerName != "bravo" || sel.model != "m-pro" || sel.adapter.Name() != "bravo" {
		t.Fatalf("selection = %#v", sel)
	}
	if sel.selection.JudgeModel != "jev-1.14.0" {
		t.Fatalf("judge model = %q", sel.selection.JudgeModel)
	}
	if len(*requests) != 1 {
		t.Fatalf("Jev requests = %d, want 1", len(*requests))
	}
	questions := (*requests)[0]["questions"].(map[string]any)
	criteria := questions["model"].(map[string]any)["criteria"].(map[string]any)
	for _, key := range []string{"alpha/m-fast", "alpha/m-pro", "bravo/m-fast", "bravo/m-pro"} {
		if _, ok := criteria[key]; !ok {
			t.Errorf("criteria missing %q: %#v", key, criteria)
		}
	}
}

func TestResolveAutoSingleCandidateSkipsJev(t *testing.T) {
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": &stubProvider{name: "alpha"}})
	p := cfg.Providers["alpha"]
	p.Models = []config.Model{{ID: "m-fast"}}
	p.DefaultModel = "m-fast"
	cfg.Providers["alpha"] = p
	jev, requests := newAutoStub(t, autoResponse("alpha/m-fast", 0.9), http.StatusOK)
	cfg.Auto = &config.Auto{
		BaseURL:        jev.URL,
		EnvKey:         "TEST_AUTO_KEY",
		Model:          "jev-latest",
		TimeoutSeconds: 5,
	}
	s := New(cfg, "test")

	sel, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), "")
	if errResult != nil {
		t.Fatalf("resolveAuto() = %s", toolResultText(t, errResult))
	}
	if sel.providerName != "alpha" || sel.model != "m-fast" {
		t.Fatalf("selection = %#v", sel)
	}
	if sel.selection.JudgeModel != "" || sel.selection.Confidence != 1 {
		t.Fatalf("selection details = %#v", sel.selection)
	}
	if len(*requests) != 0 {
		t.Fatalf("Jev requests = %d, want 0", len(*requests))
	}
}

func TestResolveAutoOmitsProviderWithoutKey(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	jev, requests := newAutoStub(t, autoResponse("alpha/m-fast", 0.8), http.StatusOK)
	cfg := autoConfig(t, jev.URL)
	t.Setenv("TEST_STUB_KEY_BRAVO", "")
	s := New(cfg, "test")

	if _, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), ""); errResult != nil {
		t.Fatalf("resolveAuto() = %s", toolResultText(t, errResult))
	}
	if len(*requests) != 1 {
		t.Fatalf("Jev requests = %d, want 1", len(*requests))
	}
	questions := (*requests)[0]["questions"].(map[string]any)
	criteria := questions["model"].(map[string]any)["criteria"].(map[string]any)
	for key := range criteria {
		if strings.HasPrefix(key, "bravo/") {
			t.Errorf("criteria offered an unavailable provider: %q", key)
		}
	}
}

func TestResolveAutoNoReachableProviderFails(t *testing.T) {
	jev, requests := newAutoStub(t, autoResponse("alpha/m-fast", 0.8), http.StatusOK)
	cfg := autoConfig(t, jev.URL)
	t.Setenv("TEST_STUB_KEY_ALPHA", "")
	t.Setenv("TEST_STUB_KEY_BRAVO", "")
	s := New(cfg, "test")

	_, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), "")
	if errResult == nil {
		t.Fatal("resolveAuto() error = nil")
	}
	text := toolResultText(t, errResult)
	for _, want := range []string{"no provider is usable", "TEST_STUB_KEY_ALPHA", "TEST_STUB_KEY_BRAVO"} {
		if !strings.Contains(text, want) {
			t.Errorf("error %q lacks %q", text, want)
		}
	}
	if len(*requests) != 0 {
		t.Fatalf("Jev requests = %d, want 0", len(*requests))
	}
}

func TestResolveAutoRejectsProviderArgument(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	jev, requests := newAutoStub(t, autoResponse("alpha/m-fast", 0.8), http.StatusOK)
	cfg := autoConfig(t, jev.URL)
	s := New(cfg, "test")

	_, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), "alpha")
	if errResult == nil || !strings.Contains(toolResultText(t, errResult), "provider must be omitted") {
		t.Fatalf("resolveAuto() = %#v, want provider omission error", errResult)
	}
	if len(*requests) != 0 {
		t.Fatalf("Jev requests = %d, want 0", len(*requests))
	}
}

func TestResolveAutoRequiresAutoSection(t *testing.T) {
	s := New(testConfig(t, &stubProvider{}), "test")
	_, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), "")
	if errResult == nil || !strings.Contains(toolResultText(t, errResult), "[auto]") {
		t.Fatalf("resolveAuto() = %#v, want missing [auto] error", errResult)
	}
}

func TestResolveAutoRequiresJevKeyWhenChoosing(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "")
	jev, requests := newAutoStub(t, autoResponse("alpha/m-fast", 0.8), http.StatusOK)
	cfg := autoConfig(t, jev.URL)
	s := New(cfg, "test")

	_, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), "")
	if errResult == nil {
		t.Fatal("resolveAuto() error = nil")
	}
	text := toolResultText(t, errResult)
	for _, want := range []string{"TEST_AUTO_KEY", "auto.env_key"} {
		if !strings.Contains(text, want) {
			t.Errorf("error %q lacks %q", text, want)
		}
	}
	if len(*requests) != 0 {
		t.Fatalf("Jev requests = %d, want 0", len(*requests))
	}
}

func TestResolveAutoReportsJevFailure(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	alpha := &stubProvider{name: "alpha"}
	bravo := &stubProvider{name: "bravo"}
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": alpha, "bravo": bravo})
	jev, _ := newAutoStub(t, `{"detail":"upstream failed"}`, http.StatusInternalServerError)
	cfg.Auto = &config.Auto{BaseURL: jev.URL, EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	sel, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), "")
	if sel != nil || errResult == nil {
		t.Fatalf("resolveAuto() = (%#v, %#v), want nil selection and tool error", sel, errResult)
	}
	text := toolResultText(t, errResult)
	for _, want := range []string{"auto model selection", "500"} {
		if !strings.Contains(text, want) {
			t.Errorf("error %q lacks %q", text, want)
		}
	}
	if len(alpha.recordedRequests()) != 0 || len(bravo.recordedRequests()) != 0 {
		t.Fatalf("a provider turn ran after a failed decision")
	}
}

func TestResolveAutoRejectsUnknownChoice(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	jev, _ := newAutoStub(t, autoResponse("nope", 0.8), http.StatusOK)
	cfg := autoConfig(t, jev.URL)
	s := New(cfg, "test")

	_, errResult := s.resolveAuto(context.Background(), "task", t.TempDir(), "")
	if errResult == nil {
		t.Fatal("resolveAuto() error = nil")
	}
	text := toolResultText(t, errResult)
	for _, want := range []string{"unknown choice", "nope"} {
		if !strings.Contains(text, want) {
			t.Errorf("error %q lacks %q", text, want)
		}
	}
}

func TestAutoCandidatesIncludesCodexWithoutKeyCheck(t *testing.T) {
	t.Setenv("TEST_STUB_KEY_API", "key")
	cfg := &config.Config{Providers: map[string]config.Provider{
		"api": {
			API:          testProviderAPI,
			BaseURL:      "test-stub://api",
			EnvKey:       "TEST_STUB_KEY_API",
			DefaultModel: "m-fast",
			Models:       []config.Model{{ID: "m-fast"}},
		},
		"codex": {
			API:          config.APICodexAppServer,
			DefaultModel: "gpt-6.1-terra",
			Models:       []config.Model{{ID: "gpt-6.1-terra"}},
		},
	}}
	s := New(cfg, "test")

	candidates, omitted := s.autoCandidates()
	if len(omitted) != 0 {
		t.Fatalf("omitted = %v, want none", omitted)
	}
	seen := map[string]bool{}
	for _, candidate := range candidates {
		seen[candidate.Provider+"/"+candidate.Model] = true
	}
	for _, want := range []string{"api/m-fast", "codex/gpt-6.1-terra"} {
		if !seen[want] {
			t.Errorf("candidates missing %q: %#v", want, candidates)
		}
	}
}
