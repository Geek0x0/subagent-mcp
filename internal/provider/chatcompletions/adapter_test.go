package chatcompletions

import (
	"context"
	"encoding/json"
	"net/http"
	"slices"
	"strings"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func newAdapter(t *testing.T, fake *testutil.FakeChat) provider.Provider {
	t.Helper()
	p, err := New("deepseek", config.Provider{API: config.APIChatCompletions, BaseURL: fake.URL}, "key")
	if err != nil {
		t.Fatal(err)
	}
	return p
}

var shellSpec = provider.ToolSpec{
	Name:        "shell",
	Description: "run",
	Parameters:  json.RawMessage(`{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}`),
}

func TestAdapterTextTurn(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Text: "hello"}})
	p := newAdapter(t, fake)
	var deltas string
	res, err := p.Turn(context.Background(), provider.TurnRequest{
		Model: "m", Effort: "high", System: "sys",
		Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
		Tools:    []provider.ToolSpec{shellSpec},
	}, func(d string) { deltas += d })
	if err != nil {
		t.Fatal(err)
	}
	if res.Text != "hello" || deltas != "hello" || res.Usage == nil || res.Usage.Total != 12 {
		t.Fatalf("Turn() = %#v deltas=%q", res, deltas)
	}
	req := fake.Request(0)
	system := req["messages"].([]any)[0].(map[string]any)
	if system["role"] != "system" || system["content"] != "sys" {
		t.Fatalf("system message = %#v", system)
	}
	if req["reasoning_effort"] != "high" || req["model"] != "m" {
		t.Fatalf("request = %#v", req)
	}
}

func TestAdapterReplaysReasoningAndToolResults(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{
		{Reasoning: "think", ToolCalls: []testutil.FakeToolCall{{ID: "c1", Name: "shell", Args: `{"command":"ls"}`}}},
		{Text: "done"},
	})
	p := newAdapter(t, fake)
	history := []provider.Message{{Role: provider.RoleUser, Text: "go"}}
	first, err := p.Turn(context.Background(), provider.TurnRequest{Model: "m", Messages: history, Tools: []provider.ToolSpec{shellSpec}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if first.Reasoning != "think" || len(first.ToolCalls) != 1 || first.ToolCalls[0].Arguments != `{"command":"ls"}` {
		t.Fatalf("first = %#v", first)
	}
	history = append(history,
		provider.Message{Role: provider.RoleAssistant, ToolCalls: first.ToolCalls, Opaque: first.Opaque},
		provider.Message{Role: provider.RoleTool, ToolCallID: "c1", Text: "a.txt", IsError: true},
	)
	if _, err := p.Turn(context.Background(), provider.TurnRequest{Model: "m", Messages: history, Tools: []provider.ToolSpec{shellSpec}}, nil); err != nil {
		t.Fatal(err)
	}
	// No System in this request, so there is no system message: [user, assistant, tool].
	messages := fake.Request(1)["messages"].([]any)
	assistant := messages[1].(map[string]any)
	if assistant["reasoning_content"] != "think" || assistant["tool_calls"] == nil {
		t.Fatalf("assistant replay = %#v", assistant)
	}
	tool := messages[2].(map[string]any)
	if tool["role"] != "tool" || tool["tool_call_id"] != "c1" || tool["content"] != "a.txt" {
		t.Fatalf("tool replay = %#v", tool)
	}
}

func TestListModelsReturnsIDs(t *testing.T) {
	fake := testutil.NewFakeChat(t, nil)
	fake.SetModels([]string{"model-a", "model-b", "model-c"})
	p := newAdapter(t, fake)
	lister, ok := p.(provider.ModelLister)
	if !ok {
		t.Fatal("chatcompletions adapter does not implement provider.ModelLister")
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
	fake := testutil.NewFakeChat(t, nil)
	fake.SetModelsStatus(http.StatusUnauthorized)
	p := newAdapter(t, fake)
	lister, ok := p.(provider.ModelLister)
	if !ok {
		t.Fatal("chatcompletions adapter does not implement provider.ModelLister")
	}
	ids, err := lister.ListModels(context.Background())
	if err == nil {
		t.Fatalf("ListModels() = %v, want non-nil error", ids)
	}
	if !strings.Contains(err.Error(), "list models") {
		t.Fatalf("ListModels() error = %v, want wrapped \"list models\" context", err)
	}
}

// TestTurnSendsConfiguredMaxOutputTokens pins that max_output_tokens reaches the
// wire. The field used to be read by the messages adapter alone, so configuring
// it on a chat-completions provider silently sent no limit at all.
func TestTurnSendsConfiguredMaxOutputTokens(t *testing.T) {
	tests := []struct {
		name      string
		configure int
		want      any // nil means the field must be absent
	}{
		{name: "configured limit is sent", configure: 4096, want: float64(4096)},
		{name: "unset leaves the field out", configure: 0, want: nil},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Text: "ok"}})
			adapter, err := New("deepseek", config.Provider{
				API: config.APIChatCompletions, BaseURL: fake.URL, MaxOutputTokens: test.configure,
			}, "key")
			if err != nil {
				t.Fatalf("New() error = %v", err)
			}
			if _, err := adapter.Turn(context.Background(), provider.TurnRequest{
				Model:    "m",
				Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
			}, nil); err != nil {
				t.Fatalf("Turn() error = %v", err)
			}

			got, present := fake.Request(0)["max_tokens"]
			if test.want == nil {
				if present {
					t.Errorf("max_tokens = %v, want the field absent when the config sets no limit", got)
				}
				return
			}
			if !present || got != test.want {
				t.Errorf("max_tokens = %v (present %v), want %v", got, present, test.want)
			}
		})
	}
}

// TestTurnRejectsMalformedToolSchema pins that a schema the adapter cannot read
// fails the turn here too. The three adapters used to disagree: responses
// rejected it, messages silently turned the tool into one with no parameters,
// and chat-completions passed it to the API unchecked.
func TestTurnRejectsMalformedToolSchema(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Text: "ok"}})
	adapter, err := New("deepseek", config.Provider{API: config.APIChatCompletions, BaseURL: fake.URL}, "key")
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}

	_, err = adapter.Turn(context.Background(), provider.TurnRequest{
		Model:    "m",
		Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
		Tools:    []provider.ToolSpec{{Name: "shell", Parameters: json.RawMessage(`{`)}},
	}, nil)
	if err == nil || !strings.Contains(err.Error(), "tool shell schema") {
		t.Fatalf("Turn() error = %v, want it to name the malformed schema", err)
	}
	if got := fake.RequestHeaderCount(); got != 0 {
		t.Errorf("requests sent = %d, want 0: a malformed schema must not reach the API", got)
	}
}

// TestTurnSendsAnObjectForAnAbsentToolSchema pins the shared schema contract on
// the wire: an absent schema means "no arguments", which has to reach the API as
// an empty object. The SDK marshals a nil Parameters as null, which the API
// rejects, while the other two adapters send an object.
func TestTurnSendsAnObjectForAnAbsentToolSchema(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Text: "ok"}})
	adapter, err := New("deepseek", config.Provider{API: config.APIChatCompletions, BaseURL: fake.URL}, "key")
	if err != nil {
		t.Fatalf("New() error = %v", err)
	}

	if _, err := adapter.Turn(context.Background(), provider.TurnRequest{
		Model:    "m",
		Messages: []provider.Message{{Role: provider.RoleUser, Text: "hi"}},
		Tools:    []provider.ToolSpec{{Name: "ping"}},
	}, nil); err != nil {
		t.Fatalf("Turn() error = %v", err)
	}

	tools, ok := fake.Request(0)["tools"].([]any)
	if !ok || len(tools) != 1 {
		t.Fatalf("tools = %#v, want one entry", fake.Request(0)["tools"])
	}
	function := tools[0].(map[string]any)["function"].(map[string]any)
	parameters, present := function["parameters"]
	if !present || parameters == nil {
		t.Fatalf("parameters = %#v (present %v), want an empty object rather than null", parameters, present)
	}
	if schema, ok := parameters.(map[string]any); !ok || len(schema) != 0 {
		t.Errorf("parameters = %#v, want {}", parameters)
	}
}
