package decision

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
)

func TestNewSharesHTTPClient(t *testing.T) {
	first := New(config.Auto{}, "first-key")
	second := New(config.Auto{}, "second-key")
	if first.httpClient != second.httpClient {
		t.Fatal("decision clients do not share an HTTP client")
	}
	if first.apiKey != "first-key" || second.apiKey != "second-key" {
		t.Fatal("decision clients do not retain their own API keys")
	}
}

func TestChooseSendsOneChoiceQuestionAndMapsAnswer(t *testing.T) {
	var requestBody map[string]any
	c := New(config.Auto{BaseURL: "https://example.com", Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	c.httpClient = &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
		if r.URL.Path != "/v1/systemone" || r.Method != http.MethodPost {
			t.Errorf("request = %s %s, want POST /v1/systemone", r.Method, r.URL.Path)
		}
		if got := r.Header.Get("Authorization"); got != "Bearer test-key" {
			t.Errorf("Authorization = %q", got)
		}
		if got := r.Header.Get("TypeSafe-Danger-Zone-Long-Context"); got != "true" {
			t.Errorf("TypeSafe-Danger-Zone-Long-Context = %q, want true", got)
		}
		_ = json.NewDecoder(r.Body).Decode(&requestBody)
		return &http.Response{StatusCode: http.StatusOK, Body: io.NopCloser(strings.NewReader(`{
			"model":"jev-1.14.0",
			"answers":{"model":{"type":"choice","choice":"c2","confidence":0.71,
				"probabilities":{"c1":0.29,"c2":0.71,"unknown":0.5}}},
			"usage":{"input_tokens":500,"output_tokens":7}}`))}, nil
	})}

	got, err := c.Choose(context.Background(), "fix the parser", "/repo",
		[]Candidate{
			{Provider: "alpha", Model: "m-fast", API: "chat-completions", Description: "fast, cheap"},
			{Provider: "bravo", Model: "org/model", API: "codex-app-server"},
		})
	if err != nil {
		t.Fatalf("Choose() error = %v", err)
	}
	if got.Provider != "bravo" || got.Model != "org/model" || got.JudgeModel != "jev-1.14.0" || got.Confidence != 0.71 || got.Probabilities["bravo/org/model"] != 0.71 || got.InputTokens != 500 {
		t.Fatalf("Selection = %#v", got)
	}
	if want := map[string]float64{"alpha/m-fast": 0.29, "bravo/org/model": 0.71}; !reflect.DeepEqual(got.Probabilities, want) {
		t.Fatalf("probabilities = %#v, want %#v", got.Probabilities, want)
	}
	state := requestBody["state"].(map[string]any)
	if state["task"] != "fix the parser" || state["cwd"] != "/repo" {
		t.Fatalf("state = %#v", state)
	}
	questions := requestBody["questions"].(map[string]any)
	q := questions["model"].(map[string]any)
	if q["type"] != "choice" || len(q["criteria"].(map[string]any)) != 2 {
		t.Fatalf("question = %#v", q)
	}
	criteria := q["criteria"].(map[string]any)
	if got := criteria["c1"]; got != "provider alpha (api chat-completions); model m-fast; fast, cheap" {
		t.Fatalf("criteria alpha = %#v", got)
	}
	if got := criteria["c2"]; got != "provider bravo (api codex-app-server); model org/model" {
		t.Fatalf("criteria bravo = %#v", got)
	}
	if requestBody["model"] != "jev-latest" {
		t.Fatalf("judge model = %#v", requestBody["model"])
	}
}

func TestChooseRejectsUnknownChoice(t *testing.T) {
	_, err := chooseAgainst(t, `{
		"model":"jev-1.14.0",
		"answers":{"model":{"type":"choice","choice":"nope","confidence":0.5,"probabilities":{}}},
		"usage":{"input_tokens":10,"output_tokens":1}}`)
	if err == nil || !strings.Contains(err.Error(), "unknown choice") || !strings.Contains(err.Error(), "nope") {
		t.Fatalf("Choose() error = %v, want unknown choice naming nope", err)
	}
}

type roundTripFunc func(*http.Request) (*http.Response, error)

func (f roundTripFunc) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }

func TestChooseClefEndpointAndEnvelope(t *testing.T) {
	c := New(config.Auto{URL: "https://example.com/ai/run/@cf/cloudflare/clef", Model: "clef"}, "key")
	c.httpClient = &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
		if r.URL.Path != "/ai/run/@cf/cloudflare/clef" {
			t.Errorf("path = %q", r.URL.Path)
		}
		if r.Header.Get("Authorization") != "Bearer key" {
			t.Error("missing authorization")
		}
		return &http.Response{StatusCode: 200, Body: io.NopCloser(strings.NewReader(`{"result":{"model":"clef","answers":{"model":{"type":"choice","choice":"c1"}},"usage":{"input_tokens":12}},"success":true}`))}, nil
	})}
	got, err := c.Choose(context.Background(), "task", "/repo", []Candidate{{Provider: "a", Model: "m"}})
	if err != nil || got.Model != "m" || got.JudgeModel != "clef" || got.InputTokens != 12 {
		t.Fatalf("Choose() = %#v, %v", got, err)
	}
}

func TestChooseCloudflareErrors(t *testing.T) {
	c := New(config.Auto{BaseURL: "https://example.com"}, "test-key")
	c.httpClient = &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
		return &http.Response{StatusCode: 400, Body: io.NopCloser(strings.NewReader(`{"errors":[{"code":123,"message":"invalid model"},{"message":"test-key secret-task"}]}`))}, nil
	})}
	_, err := c.Choose(context.Background(), "secret-task", "/repo", []Candidate{{Provider: "a", Model: "m"}})
	if err == nil || !strings.Contains(err.Error(), "invalid model") || strings.Contains(err.Error(), "test-key") || strings.Contains(err.Error(), "secret-task") {
		t.Fatalf("error = %v", err)
	}
}

func TestChooseRejectsNonChoiceAnswer(t *testing.T) {
	_, err := chooseAgainst(t, `{
		"model":"jev-1.14.0",
		"answers":{"model":{"type":"noul","noul":0.5}},
		"usage":{"input_tokens":10,"output_tokens":1}}`)
	if err == nil || !strings.Contains(err.Error(), "choice") {
		t.Fatalf("Choose() error = %v, want a non-choice error", err)
	}
}

func TestChooseReportsHTTPDetailWithoutKey(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = io.WriteString(w, `{"detail":"invalid API key"}`)
	}))
	defer server.Close()

	c := New(config.Auto{BaseURL: server.URL, Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	_, err := c.Choose(context.Background(), "task", "/repo", []Candidate{{Provider: "a", Model: "m"}})
	if err == nil {
		t.Fatal("Choose() error = nil")
	}
	if !strings.Contains(err.Error(), "401") || !strings.Contains(err.Error(), "invalid API key") {
		t.Fatalf("error = %v, want status and detail", err)
	}
	if strings.Contains(err.Error(), "test-key") {
		t.Fatalf("error leaked the API key: %v", err)
	}
}

func TestChooseReportsValidationDetail(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnprocessableEntity)
		_, _ = io.WriteString(w, `{"detail":{"loc":["body","state"],"msg":"state is too long"}}`)
	}))
	defer server.Close()

	c := New(config.Auto{BaseURL: server.URL, Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	_, err := c.Choose(context.Background(), strings.Repeat("x", 100_000), "/repo", []Candidate{{Provider: "a", Model: "m"}})
	if err == nil || !strings.Contains(err.Error(), "422") || !strings.Contains(err.Error(), "state is too long") {
		t.Fatalf("error = %v, want status and validation detail", err)
	}
}

func TestChooseTimesOut(t *testing.T) {
	release := make(chan struct{})
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		<-release
	}))
	defer server.Close()
	defer close(release)

	c := New(config.Auto{BaseURL: server.URL, Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	c.timeout = 50 * time.Millisecond
	start := time.Now()
	_, err := c.Choose(context.Background(), "task", "/repo", []Candidate{{Provider: "a", Model: "m"}})
	if err == nil || !strings.Contains(err.Error(), "context deadline exceeded") {
		t.Fatalf("error = %v, want context deadline exceeded", err)
	}
	if time.Since(start) > 2*time.Second {
		t.Fatalf("Choose() took %v, want a prompt timeout", time.Since(start))
	}
}

func TestChooseRejectsTooManyCandidates(t *testing.T) {
	requests := 0
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requests++
	}))
	defer server.Close()

	c := New(config.Auto{BaseURL: server.URL, Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	candidates := make([]Candidate, 256)
	for i := range candidates {
		candidates[i] = Candidate{Provider: "a", Model: fmt.Sprintf("m%d", i)}
	}
	_, err := c.Choose(context.Background(), "task", "/repo", candidates)
	if err == nil || !strings.Contains(err.Error(), "255") {
		t.Fatalf("error = %v, want the 255 limit", err)
	}
	if requests != 0 {
		t.Fatalf("requests = %d, want none", requests)
	}
}

func TestChooseHandlesTrailingSlash(t *testing.T) {
	var path string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		path = r.URL.Path
		_, _ = io.WriteString(w, `{
			"model":"jev-1.14.0",
			"answers":{"model":{"type":"choice","choice":"c1","confidence":1,"probabilities":{"c1":1}}},
			"usage":{"input_tokens":1,"output_tokens":1}}`)
	}))
	defer server.Close()

	c := New(config.Auto{BaseURL: server.URL + "/prefix/", Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	if _, err := c.Choose(context.Background(), "task", "/repo", []Candidate{{Provider: "a", Model: "m"}}); err != nil {
		t.Fatalf("Choose() error = %v", err)
	}
	if path != "/prefix/v1/systemone" {
		t.Fatalf("path = %q, want /prefix/v1/systemone", path)
	}
}

func TestChooseRejectsOversizedResponse(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = io.WriteString(w, strings.Repeat("x", 1<<20+1))
	}))
	defer server.Close()

	c := New(config.Auto{BaseURL: server.URL, Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	_, err := c.Choose(context.Background(), "task", "/repo", []Candidate{{Provider: "a", Model: "m"}})
	if err == nil || !strings.Contains(err.Error(), "too large") {
		t.Fatalf("error = %v, want an oversized response error", err)
	}
}

func TestChooseRejectsNoCandidates(t *testing.T) {
	c := New(config.Auto{BaseURL: "http://127.0.0.1:1", Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	_, err := c.Choose(context.Background(), "task", "/repo", nil)
	if err == nil || !strings.Contains(err.Error(), "no candidates") {
		t.Fatalf("error = %v, want no candidates", err)
	}
}

func chooseAgainst(t *testing.T, response string) (Selection, error) {
	t.Helper()
	c := New(config.Auto{BaseURL: "https://example.com", Model: "jev-latest", TimeoutSeconds: 5}, "test-key")
	c.httpClient = &http.Client{Transport: roundTripFunc(func(r *http.Request) (*http.Response, error) {
		return &http.Response{StatusCode: http.StatusOK, Body: io.NopCloser(strings.NewReader(response))}, nil
	})}
	return c.Choose(context.Background(), "task", "/repo", []Candidate{{Provider: "a", Model: "m"}})
}

func TestNewAppliesDefaults(t *testing.T) {
	c := New(config.Auto{Model: "jev-latest"}, "key")
	if c.baseURL != config.DefaultAutoBaseURL {
		t.Fatalf("baseURL = %q, want %q", c.baseURL, config.DefaultAutoBaseURL)
	}
	if c.timeout != config.DefaultAutoTimeoutSeconds*time.Second {
		t.Fatalf("timeout = %v, want %v", c.timeout, config.DefaultAutoTimeoutSeconds*time.Second)
	}
}
