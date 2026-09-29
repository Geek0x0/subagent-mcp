package responses

import (
	"context"
	"encoding/json"
	"net/http"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

var shellSpec = provider.ToolSpec{
	Name: "shell", Description: "run",
	Parameters: json.RawMessage(`{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}`),
}

func newAdapter(t *testing.T, fake *testutil.FakeResponses) provider.Provider {
	t.Helper()
	p, err := New("openai", config.Provider{API: config.APIResponses, BaseURL: fake.URL}, "k")
	if err != nil {
		t.Fatal(err)
	}
	return p
}

func itemKind(item map[string]any) string {
	kind, _ := item["type"].(string)
	if kind == "" || kind == "message" {
		role, _ := item["role"].(string)
		return "message:" + role
	}
	return kind
}

func TestResponsesTextTurn(t *testing.T) {
	fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{{
		Items:       []testutil.FakeResponseItem{{MessageText: "hello"}},
		InputTokens: 10, CachedTokens: 4, OutputTokens: 3, ReasoningTokens: 1,
	}})
	p := newAdapter(t, fake)
	var deltas string
	res, err := p.Turn(context.Background(), provider.TurnRequest{
		Model: "gpt-x", Effort: "high", System: "sys",
		Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
		Tools:    []provider.ToolSpec{shellSpec},
	}, func(d string) { deltas += d })
	if err != nil {
		t.Fatal(err)
	}
	if res.Text != "hello" || deltas != "hello" {
		t.Fatalf("res = %#v deltas = %q", res, deltas)
	}
	if *res.Usage != (provider.Usage{Input: 10, Cached: 4, Output: 3, Reasoning: 1, Total: 13}) {
		t.Fatalf("usage = %#v", res.Usage)
	}
	req := fake.Request(0)
	if req["store"] != false || req["instructions"] != "sys" || req["model"] != "gpt-x" {
		t.Fatalf("request = %#v", req)
	}
	if include := req["include"].([]any); len(include) != 1 || include[0] != "reasoning.encrypted_content" {
		t.Fatalf("include = %#v", req["include"])
	}
	if reasoning := req["reasoning"].(map[string]any); reasoning["effort"] != "high" || reasoning["summary"] != "auto" {
		t.Fatalf("reasoning = %#v", reasoning)
	}
	if tool := req["tools"].([]any)[0].(map[string]any); tool["type"] != "function" || tool["name"] != "shell" || tool["parameters"] == nil {
		t.Fatalf("tool = %#v", tool)
	}
}

func TestResponsesReplaysEveryTurn(t *testing.T) {
	fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{
		{Items: []testutil.FakeResponseItem{
			{ReasoningSummary: "plan 1", EncryptedContent: "enc-1"},
			{FunctionCallID: "call_1", FunctionName: "shell", FunctionArguments: `{"command":"ls"}`},
			{FunctionCallID: "call_2", FunctionName: "shell", FunctionArguments: `{"command":"pwd"}`},
		}},
		{Items: []testutil.FakeResponseItem{
			{ReasoningSummary: "plan 2", EncryptedContent: "enc-2"},
			{FunctionCallID: "call_3", FunctionName: "shell", FunctionArguments: `{"command":"cat a"}`},
		}},
		{Items: []testutil.FakeResponseItem{{MessageText: "done"}}},
	})
	p := newAdapter(t, fake)
	history := []provider.Message{{Role: provider.RoleUser, Text: "go"}}
	for turn := 0; turn < 3; turn++ {
		res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "gpt-x", Messages: history, Tools: []provider.ToolSpec{shellSpec}}, nil)
		if err != nil {
			t.Fatal(err)
		}
		if turn == 0 && (res.Reasoning != "plan 1" || len(res.ToolCalls) != 2 || res.ToolCalls[1].ID != "call_2" || res.ToolCalls[0].Arguments != `{"command":"ls"}`) {
			t.Fatalf("turn 0 = %#v", res)
		}
		history = append(history, provider.Message{Role: provider.RoleAssistant, Text: res.Text, ToolCalls: res.ToolCalls, Opaque: res.Opaque})
		for _, call := range res.ToolCalls {
			history = append(history, provider.Message{Role: provider.RoleTool, ToolCallID: call.ID, Text: "out-" + call.ID})
		}
	}
	input := fake.Request(2)["input"].([]any)
	var kinds []string
	for _, item := range input {
		kinds = append(kinds, itemKind(item.(map[string]any)))
	}
	want := []string{"message:user", "reasoning", "function_call", "function_call", "function_call_output", "function_call_output", "reasoning", "function_call", "function_call_output"}
	if !reflect.DeepEqual(kinds, want) {
		t.Fatalf("input item kinds = %v, want %v", kinds, want)
	}
	if input[1].(map[string]any)["encrypted_content"] != "enc-1" || input[6].(map[string]any)["encrypted_content"] != "enc-2" {
		t.Fatalf("encrypted reasoning not replayed: %#v", input)
	}
	if out := input[4].(map[string]any); out["call_id"] != "call_1" || out["output"] != "out-call_1" {
		t.Fatalf("function_call_output = %#v", out)
	}
}

func TestResponsesIncompleteAndFailed(t *testing.T) {
	for _, tc := range []struct{ status, reason, want string }{
		{"incomplete", "max_output_tokens", "max_output_tokens"},
		{"failed", "", "scripted failure"},
	} {
		fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{{ResponseStatus: tc.status, IncompleteReason: tc.reason}})
		p := newAdapter(t, fake)
		_, err := p.Turn(context.Background(), provider.TurnRequest{Model: "gpt-x", Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}}}, nil)
		if err == nil || !strings.Contains(err.Error(), tc.want) {
			t.Fatalf("%s: err = %v", tc.status, err)
		}
	}
}

// A completed response whose message carries only a refusal part must not
// complete the session with an empty answer.
func TestResponsesRefusalPartFails(t *testing.T) {
	fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{{
		Items: []testutil.FakeResponseItem{{MessageRefusal: "I cannot help with that."}},
	}})
	p := newAdapter(t, fake)
	res, err := p.Turn(context.Background(), provider.TurnRequest{
		Model: "gpt-x", Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
	}, nil)
	if err == nil {
		var text string
		if res != nil {
			text = res.Text
		}
		t.Fatalf("refusal must fail the turn, got err = nil and text %q", text)
	}
	if !strings.Contains(err.Error(), "I cannot help with that.") {
		t.Fatalf("refusal text missing from error: %v", err)
	}
}

func TestResponsesStreamError(t *testing.T) {
	fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{{
		Items:       []testutil.FakeResponseItem{{MessageText: "partial"}},
		StreamError: &testutil.FakeResponseStreamError{Code: "invalid_request_error", Message: "scripted stream failure"},
	}})
	p := newAdapter(t, fake)
	_, err := p.Turn(context.Background(), provider.TurnRequest{Model: "gpt-x", Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}}}, nil)
	if err == nil || !strings.Contains(err.Error(), "invalid_request_error") || !strings.Contains(err.Error(), "scripted stream failure") {
		t.Fatalf("err = %v, want both error code and message", err)
	}
}

func TestResponsesHTTPError(t *testing.T) {
	fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{{Status: 400}})
	p := newAdapter(t, fake)
	if _, err := p.Turn(context.Background(), provider.TurnRequest{Model: "gpt-x", Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}}}, nil); err == nil {
		t.Fatal("want error")
	}
}

func TestListModelsReturnsIDs(t *testing.T) {
	fake := testutil.NewFakeResponses(t, nil)
	fake.SetModels([]string{"model-a", "model-b", "model-c"})
	p := newAdapter(t, fake)
	lister, ok := p.(provider.ModelLister)
	if !ok {
		t.Fatal("responses adapter does not implement provider.ModelLister")
	}
	ids, err := lister.ListModels(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(ids, []string{"model-a", "model-b", "model-c"}) {
		t.Fatalf("ListModels() = %v, want [model-a model-b model-c]", ids)
	}
}

func TestListModelsHTTPError(t *testing.T) {
	fake := testutil.NewFakeResponses(t, nil)
	fake.SetModelsStatus(http.StatusUnauthorized)
	p := newAdapter(t, fake)
	lister, ok := p.(provider.ModelLister)
	if !ok {
		t.Fatal("responses adapter does not implement provider.ModelLister")
	}
	ids, err := lister.ListModels(context.Background())
	if err == nil {
		t.Fatalf("ListModels() = %v, want non-nil error", ids)
	}
	if !strings.Contains(err.Error(), "list models") {
		t.Fatalf("ListModels() error = %v, want wrapped \"list models\" context", err)
	}
}
