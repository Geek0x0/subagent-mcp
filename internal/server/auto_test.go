package server

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"

	"github.com/mark3labs/mcp-go/mcp"
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
	jev, requests := newAutoStub(t, autoResponse("c4", 0.9), http.StatusOK)
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
	for _, key := range []string{"c1", "c2", "c3", "c4"} {
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
	jev, requests := newAutoStub(t, autoResponse("c1", 0.9), http.StatusOK)
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
	if meta := sel.meta(); meta["skipped"] != true {
		t.Fatalf("selection metadata = %#v, want skipped", meta)
	}
	if len(*requests) != 0 {
		t.Fatalf("Jev requests = %d, want 0", len(*requests))
	}
}

func TestResolveAutoOmitsProviderWithoutKey(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	jev, requests := newAutoStub(t, autoResponse("c1", 0.8), http.StatusOK)
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
	for _, description := range criteria {
		if strings.Contains(description.(string), "provider bravo ") {
			t.Errorf("criteria offered an unavailable provider: %q", description)
		}
	}
}

func TestResolveAutoNoReachableProviderFails(t *testing.T) {
	jev, requests := newAutoStub(t, autoResponse("c1", 0.8), http.StatusOK)
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
	jev, requests := newAutoStub(t, autoResponse("c1", 0.8), http.StatusOK)
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
	jev, requests := newAutoStub(t, autoResponse("c1", 0.8), http.StatusOK)
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

func TestAutoCandidatesDefaultDescriptions(t *testing.T) {
	models := []config.Model{
		{ID: "claude-opus-5"}, {ID: "claude-opus-5-5"},
		{ID: "claude-sonnet-5"}, {ID: "claude-sonnet-5-5"}, {ID: "claude-haiku-5-5"},
		{ID: "gpt-6-astra"}, {ID: "gpt-6.1-sol"}, {ID: "gpt-6.1-terra"}, {ID: "gpt-6-luna"},
		{ID: "deepseek-v4.1-flash"}, {ID: "deepseek-v4-flash"}, {ID: "gemini-3.6-flash"},
		{ID: "gateway/anthropic/claude-opus-5"},
		{ID: "anthropic/claude-sonnet-5", Description: "my configured description"},
		{ID: "unknown/model"},
		{ID: "glm-5.1"}, {ID: "glm-5.3"}, {ID: "glm-5.3-flash"},
		{ID: "kimi-k3"}, {ID: "kimi-k2.7-code"}, {ID: "kimi-k2.7-code-highspeed"}, {ID: "kimi-k2.6"},
		{ID: "mimo-v2.6-pro"}, {ID: "mimo-v2.6-flash"},
	}
	s := New(&config.Config{Providers: map[string]config.Provider{
		"codex": {API: config.APICodexAppServer, Models: models},
	}}, "test")
	candidates, _ := s.autoCandidates()
	for i := 0; i < 12; i++ {
		if candidates[i].Description == "" {
			t.Errorf("model %q has no default description", candidates[i].Model)
		}
	}
	if candidates[12].Description != candidates[0].Description {
		t.Errorf("prefixed description = %q, want %q", candidates[12].Description, candidates[0].Description)
	}
	if candidates[13].Description != "my configured description" {
		t.Errorf("configured description = %q", candidates[13].Description)
	}
	if candidates[14].Description != "" {
		t.Errorf("unknown description = %q, want empty", candidates[14].Description)
	}
	for _, candidate := range candidates[15:] {
		if candidate.Description == "" {
			t.Errorf("model %q has no default description", candidate.Model)
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

func startStructured(t *testing.T, result *mcp.CallToolResult) map[string]any {
	t.Helper()
	structured, ok := result.StructuredContent.(map[string]any)
	if !ok {
		t.Fatalf("StructuredContent = %#v, want map[string]any", result.StructuredContent)
	}
	return structured
}

func TestHandleStartAutoDispatchesChosenModel(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	alpha := &stubProvider{name: "alpha"}
	bravo := &stubProvider{name: "bravo", turns: []stubTurn{{result: &provider.TurnResult{Text: "chosen result"}}}}
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": alpha, "bravo": bravo})
	for name, model := range map[string]string{"alpha": "m-fast", "bravo": "m-pro"} {
		p := cfg.Providers[name]
		p.Models = []config.Model{{ID: model}}
		p.DefaultModel = model
		cfg.Providers[name] = p
	}
	jev, _ := newAutoStub(t, `{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"c2","confidence":0.9,"probabilities":{"c1":0.1,"c2":0.9}}},"usage":{"input_tokens":42,"output_tokens":3}}`, http.StatusOK)
	cfg.Auto = &config.Auto{BaseURL: jev.URL, EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "fix the parser",
		"cwd":    t.TempDir(),
		"model":  "auto",
	}))
	if err != nil || result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want success; text=%q", result, err, toolResultText(t, result))
	}
	if got := toolResultText(t, result); got != "chosen result" {
		t.Fatalf("result text = %q", got)
	}
	if len(alpha.recordedRequests()) != 0 || len(bravo.recordedRequests()) != 1 {
		t.Fatalf("provider turns = alpha %d, bravo %d", len(alpha.recordedRequests()), len(bravo.recordedRequests()))
	}
	structured := startStructured(t, result)
	if structured["provider"] != "bravo" || structured["model"] != "m-pro" {
		t.Fatalf("start structured = %#v", structured)
	}
	auto := structured["auto"].(map[string]any)
	if auto["judge_model"] != "jev-1.14.0" || auto["confidence"] != 0.9 {
		t.Fatalf("auto = %#v", auto)
	}
	if auto["input_tokens"] != 42 || auto["output_tokens"] != 3 {
		t.Fatalf("auto token usage = %#v", auto)
	}
	probabilities, ok := auto["probabilities"].(map[string]float64)
	if !ok || probabilities["alpha/m-fast"] != 0.1 || probabilities["bravo/m-pro"] != 0.9 {
		t.Fatalf("auto probabilities = %#v", auto["probabilities"])
	}
	if _, ok := auto["omitted"]; ok {
		t.Fatalf("auto = %#v, want no omitted providers", auto)
	}
	if _, ok := auto["skipped"]; ok {
		t.Fatalf("auto = %#v, Jev was not skipped", auto)
	}
}

func TestHandleStartAutoSingleCandidateNeedsNoJevKey(t *testing.T) {
	cfg := testMultiConfig(t, map[string]provider.Provider{
		"alpha": &stubProvider{name: "alpha", turns: []stubTurn{{result: &provider.TurnResult{Text: "single"}}}},
	})
	p := cfg.Providers["alpha"]
	p.Models = []config.Model{{ID: "m-fast"}}
	p.DefaultModel = "m-fast"
	cfg.Providers["alpha"] = p
	cfg.Auto = &config.Auto{BaseURL: "http://127.0.0.1:1", EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto",
	}))
	if err != nil || result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want success; text=%q", result, err, toolResultText(t, result))
	}
	structured := startStructured(t, result)
	if structured["provider"] != "alpha" || structured["model"] != "m-fast" {
		t.Fatalf("start structured = %#v", structured)
	}
	auto := structured["auto"].(map[string]any)
	if auto["skipped"] != true {
		t.Fatalf("auto = %#v, want skipped", auto)
	}
	for _, key := range []string{"judge_model", "confidence", "probabilities", "input_tokens", "output_tokens", "omitted"} {
		if _, ok := auto[key]; ok {
			t.Errorf("auto = %#v, unexpected %s", auto, key)
		}
	}
}

func TestHandleStartAutoReportsOmittedProvider(t *testing.T) {
	home := t.TempDir()
	t.Setenv("CODEX_HOME", home)
	t.Setenv("SUBAGENT_MCP_ROLLOUT", "")
	cfg := testMultiConfig(t, map[string]provider.Provider{
		"alpha": &stubProvider{name: "alpha", turns: []stubTurn{{result: &provider.TurnResult{Text: "single"}}}},
		"bravo": &stubProvider{name: "bravo"},
	})
	p := cfg.Providers["alpha"]
	p.Models = []config.Model{{ID: "m-fast"}}
	p.DefaultModel = "m-fast"
	cfg.Providers["alpha"] = p
	t.Setenv("TEST_STUB_KEY_BRAVO", "")
	cfg.Auto = &config.Auto{BaseURL: "http://127.0.0.1:1", EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto",
	}))
	if err != nil || result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want success", result, err)
	}
	auto := startStructured(t, result)["auto"].(map[string]any)
	omitted, ok := auto["omitted"].([]string)
	if !ok || len(omitted) != 1 || !strings.Contains(omitted[0], "bravo") || !strings.Contains(omitted[0], "TEST_STUB_KEY_BRAVO") {
		t.Fatalf("auto = %#v, want missing bravo key reason", auto)
	}
	matches, err := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "rollout-*-"+toolResultThreadID(t, result)+".jsonl"))
	if err != nil || len(matches) != 1 {
		t.Fatalf("rollout files = %v, error = %v", matches, err)
	}
	data, err := os.ReadFile(matches[0])
	if err != nil {
		t.Fatal(err)
	}
	var meta struct {
		Payload struct {
			ModelSelection map[string]any `json:"model_selection"`
		} `json:"payload"`
	}
	if err := json.Unmarshal([]byte(strings.SplitN(string(data), "\n", 2)[0]), &meta); err != nil {
		t.Fatal(err)
	}
	selection := meta.Payload.ModelSelection
	rolloutOmitted, ok := selection["omitted"].([]any)
	if selection["skipped"] != true || !ok || len(rolloutOmitted) != 1 || rolloutOmitted[0] != omitted[0] {
		t.Fatalf("rollout model_selection = %#v", selection)
	}
	for _, key := range []string{"judge_model", "confidence", "probabilities", "input_tokens", "output_tokens"} {
		if _, ok := selection[key]; ok {
			t.Errorf("rollout model_selection = %#v, unexpected %s", selection, key)
		}
	}
}

func TestHandleStartAutoFailureCreatesNoSession(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	alpha := &stubProvider{name: "alpha"}
	bravo := &stubProvider{name: "bravo"}
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": alpha, "bravo": bravo})
	jev, _ := newAutoStub(t, `{"detail":"upstream failed"}`, http.StatusInternalServerError)
	cfg.Auto = &config.Auto{BaseURL: jev.URL, EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto",
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	text := toolResultText(t, result)
	for _, want := range []string{"auto model selection", "500"} {
		if !strings.Contains(text, want) {
			t.Errorf("error %q lacks %q", text, want)
		}
	}
	if len(alpha.recordedRequests()) != 0 || len(bravo.recordedRequests()) != 0 {
		t.Fatal("a provider turn ran after a failed decision")
	}
	if result.StructuredContent != nil {
		t.Fatalf("StructuredContent = %#v, want none before a session exists", result.StructuredContent)
	}
}

func TestHandleStartAutoExecutionErrorKeepsMetadata(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	bravo := &stubProvider{name: "bravo", turns: []stubTurn{{err: errors.New("boom")}}}
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": &stubProvider{name: "alpha"}, "bravo": bravo})
	jev, _ := newAutoStub(t, autoResponse("c4", 0.9), http.StatusOK)
	cfg.Auto = &config.Auto{BaseURL: jev.URL, EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto",
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	structured := startStructured(t, result)
	if structured["threadId"] == "" || structured["provider"] != "bravo" || structured["model"] != "m-pro" {
		t.Fatalf("start structured = %#v", structured)
	}
	if _, ok := structured["auto"]; !ok {
		t.Fatalf("start structured = %#v, want auto metadata", structured)
	}
}

func TestHandleStartAutoDoesNotCallJevOnInvalidArgs(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	cfg := testConfig(t, &stubProvider{})
	jev, requests := newAutoStub(t, autoResponse("c1", 0.9), http.StatusOK)
	cfg.Auto = &config.Auto{BaseURL: jev.URL, EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto", "sandbox": "bogus",
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	if !strings.Contains(toolResultText(t, result), "read-only") {
		t.Fatalf("error = %q", toolResultText(t, result))
	}
	if len(*requests) != 0 {
		t.Fatalf("Jev requests = %d, want 0", len(*requests))
	}
}

func TestHandleStartAutoValidatesWritableRootsBeforeJev(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	jev, requests := newAutoStub(t, autoResponse("c1", 0.9), http.StatusOK)
	cfg := autoConfig(t, jev.URL)
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task",
		"cwd":    t.TempDir(),
		"model":  "auto",
		"config": map[string]any{"writable_roots": []any{"relative/path"}},
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	if text := toolResultText(t, result); !strings.Contains(text, "writable_roots") {
		t.Fatalf("error = %q, want writable_roots validation", text)
	}
	if len(*requests) != 0 {
		t.Fatalf("Jev requests = %d, want none for invalid local config", len(*requests))
	}
}

func TestHandleStartAutoHonorsRunDeadlineDuringJevSelection(t *testing.T) {
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	var requestStarted chan struct{}
	requestStarted = make(chan struct{})
	jev := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		close(requestStarted)
		time.Sleep(1100 * time.Millisecond)
		_, _ = io.WriteString(w, autoResponse("c1", 0.9))
	}))
	t.Cleanup(jev.Close)
	cfg := autoConfig(t, jev.URL)
	cfg.Auto.TimeoutSeconds = 3
	s := New(cfg, "test")

	started := time.Now()
	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task",
		"cwd":    t.TempDir(),
		"model":  "auto",
		"config": map[string]any{"run_deadline_seconds": float64(1)},
	}))
	if err != nil || !result.IsError || !strings.Contains(toolResultText(t, result), "deadline") {
		t.Fatalf("handleStart() = (%#v, %v), want a deadline tool error", result, err)
	}
	select {
	case <-requestStarted:
	case <-time.After(500 * time.Millisecond):
		t.Fatal("Jev request did not start")
	}
	if elapsed := time.Since(started); elapsed > 1500*time.Millisecond {
		t.Fatalf("handleStart() took %s, want run deadline to bound Jev selection", elapsed)
	}
}

func TestHandleStartExplicitKeepsProviderErrorFirst(t *testing.T) {
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": &stubProvider{}, "bravo": &stubProvider{}})
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "provider": "bogus", "sandbox": "bogus",
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	text := toolResultText(t, result)
	if !strings.Contains(text, "unknown provider") {
		t.Fatalf("error = %q, want unknown provider first", text)
	}
	if strings.Contains(text, "read-only") {
		t.Fatalf("error = %q, want the provider error to win", text)
	}
}

func TestHandleStartExplicitResultReportsProviderAndModel(t *testing.T) {
	alpha := &stubProvider{name: "alpha", turns: []stubTurn{{result: &provider.TurnResult{Text: "ok"}}}}
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": alpha, "bravo": &stubProvider{name: "bravo"}})
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "provider": "alpha", "model": "m-pro",
	}))
	if err != nil || result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want success", result, err)
	}
	structured := startStructured(t, result)
	if structured["provider"] != "alpha" || structured["model"] != "m-pro" {
		t.Fatalf("start structured = %#v", structured)
	}
	if _, ok := structured["auto"]; ok {
		t.Fatalf("start structured = %#v, want no auto key", structured)
	}
}

func TestHandleStartAutoWritesModelSelectionToRollout(t *testing.T) {
	home := t.TempDir()
	t.Setenv("CODEX_HOME", home)
	t.Setenv("SUBAGENT_MCP_ROLLOUT", "")
	t.Setenv("TEST_AUTO_KEY", "jev-key")
	cfg := testConfig(t, &stubProvider{turns: []stubTurn{{result: &provider.TurnResult{Text: "ok"}}}})
	jev, _ := newAutoStub(t, autoResponse("c2", 0.9), http.StatusOK)
	cfg.Auto = &config.Auto{BaseURL: jev.URL, EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")

	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto",
	}))
	if err != nil || result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want success", result, err)
	}
	threadID := toolResultThreadID(t, result)
	matches, _ := filepath.Glob(filepath.Join(home, "sessions", "*", "*", "*", "rollout-*-"+threadID+".jsonl"))
	if len(matches) != 1 {
		t.Fatalf("rollout files = %v", matches)
	}
	data, err := os.ReadFile(matches[0])
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(string(data)), "\n")
	var meta struct {
		Type    string         `json:"type"`
		Payload map[string]any `json:"payload"`
	}
	if err := json.Unmarshal([]byte(lines[0]), &meta); err != nil {
		t.Fatal(err)
	}
	selection, ok := meta.Payload["model_selection"].(map[string]any)
	if !ok {
		t.Fatalf("session_meta = %#v", meta.Payload)
	}
	if selection["provider"] != "deepseek" || selection["model"] != "m-pro" || selection["judge_model"] != "jev-1.14.0" || selection["confidence"] != 0.9 {
		t.Fatalf("model_selection = %#v", selection)
	}
}

func TestHandleStartAutoMissingSectionBeforeSandbox(t *testing.T) {
	cfg := testConfig(t, &stubProvider{})
	s := New(cfg, "test")
	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto", "sandbox": "bogus",
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	if text := toolResultText(t, result); !strings.Contains(text, "[auto]") {
		t.Fatalf("error = %q, want the missing [auto] section first", text)
	}
}

func TestHandleStartAutoMissingSectionBeforeProvider(t *testing.T) {
	cfg := testConfig(t, &stubProvider{})
	cfg.Path = "/test/config.toml"
	s := New(cfg, "test")
	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "auto", "provider": "x",
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	want := fmt.Sprintf("model \"auto\" requires an [auto] section in %s", cfg.Path)
	if text := toolResultText(t, result); text != want {
		t.Fatalf("error = %q, want %q", text, want)
	}
}

func TestResolveAutoMissingSectionBeforeProvider(t *testing.T) {
	cfg := testConfig(t, &stubProvider{})
	cfg.Path = "/test/config.toml"
	s := New(cfg, "test")
	selection, result := s.resolveAuto(context.Background(), "task", "", "x")
	if selection != nil || result == nil || !result.IsError {
		t.Fatalf("resolveAuto() = (%#v, %#v), want a tool error", selection, result)
	}
	want := fmt.Sprintf("model \"auto\" requires an [auto] section in %s", cfg.Path)
	if text := toolResultText(t, result); text != want {
		t.Fatalf("error = %q, want %q", text, want)
	}
}

func TestHandleStartExplicitModelStillNeedsProviderUnderAuto(t *testing.T) {
	cfg := testMultiConfig(t, map[string]provider.Provider{"alpha": &stubProvider{}, "bravo": &stubProvider{}})
	cfg.Auto = &config.Auto{BaseURL: "https://api.typesafe.ai", EnvKey: "TEST_AUTO_KEY", Model: "jev-latest", TimeoutSeconds: 5}
	s := New(cfg, "test")
	result, err := s.handleStart(context.Background(), callToolRequest("subagent", map[string]any{
		"prompt": "task", "cwd": t.TempDir(), "model": "m-pro",
	}))
	if err != nil || !result.IsError {
		t.Fatalf("handleStart() = (%#v, %v), want a tool error", result, err)
	}
	if text := toolResultText(t, result); !strings.Contains(text, "provider is required") {
		t.Fatalf("error = %q, want provider required", text)
	}
}
