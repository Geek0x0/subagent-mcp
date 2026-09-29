package messages

import (
	"context"
	"encoding/json"
	"fmt"
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

func newAdapter(t *testing.T, fake *testutil.FakeMessages, maxTokens int) provider.Provider {
	t.Helper()
	p, err := New("anthropic", config.Provider{API: config.APIMessages, BaseURL: fake.URL, MaxOutputTokens: maxTokens}, "k")
	if err != nil {
		t.Fatal(err)
	}
	return p
}

func userOnly() []provider.Message { return []provider.Message{{Role: provider.RoleUser, Text: "hi"}} }

func TestMessagesTextTurnRequestShape(t *testing.T) {
	fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{{
		Blocks:      []testutil.FakeBlock{{Thinking: "hmm", Signature: "sig-1"}, {Text: "hello"}},
		InputTokens: 10, CacheReadTokens: 5, CacheCreationTokens: 2, OutputTokens: 3,
	}})
	p := newAdapter(t, fake, 64000)
	var deltas string
	res, err := p.Turn(context.Background(), provider.TurnRequest{
		Model: "claude-x", Effort: "xhigh", System: "sys", Messages: userOnly(), Tools: []provider.ToolSpec{shellSpec},
	}, func(d string) { deltas += d })
	if err != nil {
		t.Fatal(err)
	}
	if res.Text != "hello" || deltas != "hello" || res.Reasoning != "hmm" {
		t.Fatalf("res = %#v deltas = %q", res, deltas)
	}
	if *res.Usage != (provider.Usage{Input: 17, Cached: 5, Output: 3, Total: 20}) {
		t.Fatalf("usage = %#v", res.Usage)
	}
	req := fake.Request(0)
	if req["model"] != "claude-x" || req["max_tokens"] != float64(64000) || req["stream"] != true {
		t.Fatalf("request = %#v", req)
	}
	if thinking := req["thinking"].(map[string]any); thinking["type"] != "adaptive" || thinking["display"] != "summarized" {
		t.Fatalf("thinking = %#v", thinking)
	}
	if req["output_config"].(map[string]any)["effort"] != "xhigh" {
		t.Fatalf("output_config = %#v", req["output_config"])
	}
	if req["cache_control"].(map[string]any)["type"] != "ephemeral" {
		t.Fatalf("cache_control = %#v", req["cache_control"])
	}
	if tool := req["tools"].([]any)[0].(map[string]any); tool["name"] != "shell" || tool["eager_input_streaming"] != true || tool["input_schema"] == nil {
		t.Fatalf("tool = %#v", tool)
	}
	if system := req["system"].([]any)[0].(map[string]any); system["text"] != "sys" {
		t.Fatalf("system = %#v", system)
	}
}

func TestMessagesReplaysThinkingAndGroupsToolResults(t *testing.T) {
	fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{
		{Blocks: []testutil.FakeBlock{
			{Thinking: "plan", Signature: "sig-1"},
			{ToolID: "tu_1", ToolName: "shell", ToolInputJSON: `{"command":"ls"}`},
			{ToolID: "tu_2", ToolName: "shell", ToolInputJSON: `{"command":"pwd"}`},
		}},
		{Blocks: []testutil.FakeBlock{
			{Thinking: "again", Signature: "sig-2"},
			{ToolID: "tu_3", ToolName: "shell", ToolInputJSON: `{"command":"cat a"}`},
		}},
		{Blocks: []testutil.FakeBlock{{Text: "done"}}},
	})
	p := newAdapter(t, fake, 1000)
	history := []provider.Message{{Role: provider.RoleUser, Text: "go"}}
	for turn := 0; turn < 3; turn++ {
		res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: history, Tools: []provider.ToolSpec{shellSpec}}, nil)
		if err != nil {
			t.Fatal(err)
		}
		if turn == 0 && (len(res.ToolCalls) != 2 || res.ToolCalls[0].Arguments != `{"command":"ls"}`) {
			t.Fatalf("turn 0 = %#v", res)
		}
		history = append(history, provider.Message{Role: provider.RoleAssistant, Text: res.Text, ToolCalls: res.ToolCalls, Opaque: res.Opaque})
		for i, call := range res.ToolCalls {
			history = append(history, provider.Message{Role: provider.RoleTool, ToolCallID: call.ID, Text: "out-" + call.ID, IsError: i == 1})
		}
	}
	messages := fake.Request(2)["messages"].([]any)
	if len(messages) != 5 {
		t.Fatalf("messages = %d, want 5 (user, assistant, user results, assistant, user results)", len(messages))
	}
	if first := messages[1].(map[string]any)["content"].([]any)[0].(map[string]any); first["type"] != "thinking" || first["signature"] != "sig-1" {
		t.Fatalf("thinking not replayed: %#v", first)
	}
	if second := messages[3].(map[string]any)["content"].([]any)[0].(map[string]any); second["signature"] != "sig-2" {
		t.Fatalf("second thinking not replayed: %#v", second)
	}
	results := messages[2].(map[string]any)["content"].([]any)
	if len(results) != 2 {
		t.Fatalf("tool results not grouped in one message: %#v", results)
	}
	r0, r1 := results[0].(map[string]any), results[1].(map[string]any)
	if r0["type"] != "tool_result" || r0["tool_use_id"] != "tu_1" || r1["tool_use_id"] != "tu_2" || r1["is_error"] != true {
		t.Fatalf("tool results = %#v", results)
	}
}

func TestMessagesTruncatedToolInputSurfacesVerbatim(t *testing.T) {
	fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{{
		Blocks: []testutil.FakeBlock{{ToolID: "tu_1", ToolName: "shell", ToolInputJSON: `{"command":"ls`}},
	}})
	p := newAdapter(t, fake, 1000)
	res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: userOnly(), Tools: []provider.ToolSpec{shellSpec}}, nil)
	if err != nil {
		t.Fatalf("adapter must not fail on bad tool JSON (the runner rejects it): %v", err)
	}
	if len(res.ToolCalls) != 1 || json.Valid([]byte(res.ToolCalls[0].Arguments)) {
		t.Fatalf("truncated input must reach the runner as invalid JSON: %#v", res.ToolCalls)
	}
}

func TestMessagesStopReasons(t *testing.T) {
	for _, tc := range []struct {
		msg  testutil.FakeMessage
		want string
	}{
		{testutil.FakeMessage{StopReason: "refusal", RefusalCategory: "cyber", Blocks: []testutil.FakeBlock{{Text: "no"}}}, "cyber"},
		{testutil.FakeMessage{StopReason: "max_tokens", Blocks: []testutil.FakeBlock{{Text: "cut"}}}, "max_output_tokens"},
		{testutil.FakeMessage{Status: 400}, "scripted failure"},
	} {
		fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{tc.msg})
		p := newAdapter(t, fake, 1000)
		_, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: userOnly()}, nil)
		if err == nil || !strings.Contains(err.Error(), tc.want) {
			t.Fatalf("stop %q: err = %v, want %q", tc.msg.StopReason, err, tc.want)
		}
	}
}

// An Anthropic reply with content: [] and stop_reason end_turn is a legal final
// answer but never a legal request message: replaying it would 400 every later
// turn on the session.
func TestMessagesEmptyEndTurnIsNeverReplayed(t *testing.T) {
	fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{
		{}, // no blocks: content: [], stop_reason end_turn
		{Blocks: []testutil.FakeBlock{{Text: "second answer"}}},
	})
	p := newAdapter(t, fake, 1000)
	history := []provider.Message{{Role: provider.RoleUser, Text: "first"}}
	res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: history}, nil)
	if err != nil {
		t.Fatalf("empty end_turn must be accepted as a final answer: %v", err)
	}
	if res.Text != "" || len(res.ToolCalls) != 0 {
		t.Fatalf("res = %#v, want an empty answer", res)
	}
	history = append(history,
		provider.Message{Role: provider.RoleAssistant, Text: res.Text, Opaque: res.Opaque},
		provider.Message{Role: provider.RoleUser, Text: "second"},
	)
	res2, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: history}, nil)
	if err != nil {
		t.Fatalf("second turn must still succeed: %v", err)
	}
	if res2.Text != "second answer" {
		t.Fatalf("second turn text = %q", res2.Text)
	}
	messages := fake.Request(1)["messages"].([]any)
	for i, entry := range messages {
		message := entry.(map[string]any)
		if message["role"] != "assistant" {
			continue
		}
		content, _ := message["content"].([]any)
		if len(content) == 0 {
			t.Fatalf("request 1 message %d replays an empty assistant message: %#v", i, message)
		}
		for _, entry := range content {
			if block := entry.(map[string]any); block["type"] == "text" && block["text"] == "" {
				t.Fatalf("request 1 message %d replays an empty text block: %#v", i, block)
			}
		}
	}
}

func TestMessagesContextWindowExceededFails(t *testing.T) {
	fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{{
		StopReason: "model_context_window_exceeded",
		Blocks:     []testutil.FakeBlock{{Text: "overflow"}},
	}})
	p := newAdapter(t, fake, 1000)
	res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: userOnly()}, nil)
	if err == nil {
		var text string
		if res != nil {
			text = res.Text
		}
		t.Fatalf("model_context_window_exceeded must fail the turn, got err = nil and text %q", text)
	}
	if !strings.Contains(err.Error(), "context window") {
		t.Fatalf("err = %v, want a context-window error", err)
	}
}

// A stream cut before message_delta/message_stop leaves StopReason empty and
// the text block's wire JSON unrefreshed; neither may surface as a success —
// and above all no empty text block may be stored for replay.
func TestMessagesTruncatedStreamFails(t *testing.T) {
	for _, tc := range []struct {
		name string
		cut  string
	}{
		{name: "cut after text deltas", cut: "delta"},
		{name: "cut after block start", cut: "block"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{{
				Blocks:         []testutil.FakeBlock{{Text: "half an answer"}},
				TruncateStream: tc.cut,
			}})
			p := newAdapter(t, fake, 1000)
			var deltas string
			res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: userOnly()},
				func(d string) { deltas += d })
			if res != nil && strings.Contains(string(res.Opaque), `"text":""`) {
				t.Fatalf("replay payload contains an empty text block: %s", res.Opaque)
			}
			if err == nil {
				var text string
				if res != nil {
					text = res.Text
				}
				t.Fatalf("truncated stream must fail the turn, got err = nil and text %q", text)
			}
			if !strings.Contains(err.Error(), "stop signal") {
				t.Fatalf("err = %v, want a missing-stop-signal error", err)
			}
			if tc.cut == "delta" {
				if !strings.Contains(err.Error(), "half an answer") {
					t.Fatalf("partial text dropped from the error: %v", err)
				}
				if deltas != "half an answer" {
					t.Fatalf("deltas = %q, want the streamed partial text", deltas)
				}
			}
		})
	}
}

func TestMessagesPauseTurnContinues(t *testing.T) {
	fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{
		{StopReason: "pause_turn", Blocks: []testutil.FakeBlock{{Text: "part one "}}},
		{Blocks: []testutil.FakeBlock{{Text: "part two"}}},
	})
	p := newAdapter(t, fake, 1000)
	res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: userOnly()}, nil)
	if err != nil || !strings.Contains(res.Text, "part one") || !strings.Contains(res.Text, "part two") {
		t.Fatalf("res = %#v err = %v", res, err)
	}
	second := fake.Request(1)["messages"].([]any)
	if len(second) != 2 || second[1].(map[string]any)["role"] != "assistant" {
		t.Fatalf("pause_turn continuation messages = %#v", second)
	}
}

func TestMessagesPauseTurnExhaustsContinuations(t *testing.T) {
	scripted := make([]testutil.FakeMessage, 0, maxPauseContinuations+1)
	for i := 0; i <= maxPauseContinuations; i++ {
		scripted = append(scripted, testutil.FakeMessage{
			StopReason: "pause_turn",
			Blocks:     []testutil.FakeBlock{{Text: fmt.Sprintf("part %d ", i)}},
		})
	}
	fake := testutil.NewFakeMessages(t, scripted)
	p := newAdapter(t, fake, 1000)
	_, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: userOnly()}, nil)
	if err == nil || !strings.Contains(err.Error(), "still paused") {
		t.Fatalf("err = %v, want a still-paused error", err)
	}
	if got := fake.RequestCount(); got != maxPauseContinuations+1 {
		t.Fatalf("requests = %d, want %d", got, maxPauseContinuations+1)
	}
}

func TestMessagesMultiDeltaAndEmptyToolInput(t *testing.T) {
	fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{
		{Blocks: []testutil.FakeBlock{
			{ToolID: "tu_1", ToolName: "shell", ToolInputChunks: []string{`{"com`, `mand":"l`, `s"}`}},
			{ToolID: "tu_2", ToolName: "noargs"},
		}},
		{Blocks: []testutil.FakeBlock{{Text: "done"}}},
	})
	p := newAdapter(t, fake, 1000)
	res, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: userOnly(), Tools: []provider.ToolSpec{shellSpec}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.ToolCalls) != 2 || res.ToolCalls[0].Arguments != `{"command":"ls"}` || res.ToolCalls[1].Arguments != `{}` {
		t.Fatalf("tool calls = %#v", res.ToolCalls)
	}

	history := []provider.Message{{Role: provider.RoleUser, Text: "hi"}}
	history = append(history, provider.Message{Role: provider.RoleAssistant, Text: res.Text, ToolCalls: res.ToolCalls, Opaque: res.Opaque})
	for _, call := range res.ToolCalls {
		history = append(history, provider.Message{Role: provider.RoleTool, ToolCallID: call.ID, Text: "out"})
	}
	if _, err := p.Turn(context.Background(), provider.TurnRequest{Model: "claude-x", Messages: history, Tools: []provider.ToolSpec{shellSpec}}, nil); err != nil {
		t.Fatal(err)
	}

	content := fake.Request(1)["messages"].([]any)[1].(map[string]any)["content"].([]any)
	if len(content) != 2 {
		t.Fatalf("replayed assistant content = %#v", content)
	}
	if block := content[0].(map[string]any); block["type"] != "tool_use" || block["id"] != "tu_1" || !reflect.DeepEqual(block["input"], map[string]any{"command": "ls"}) {
		t.Fatalf("replayed tu_1 block = %#v", block)
	}
	if block := content[1].(map[string]any); block["id"] != "tu_2" || !reflect.DeepEqual(block["input"], map[string]any{}) {
		t.Fatalf("replayed tu_2 block = %#v", block)
	}
}

func TestListModelsReturnsIDs(t *testing.T) {
	fake := testutil.NewFakeMessages(t, nil)
	fake.SetModels([]string{"model-a", "model-b", "model-c"})
	p := newAdapter(t, fake, 1000)
	lister, ok := p.(provider.ModelLister)
	if !ok {
		t.Fatal("messages adapter does not implement provider.ModelLister")
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
	fake := testutil.NewFakeMessages(t, nil)
	fake.SetModelsStatus(http.StatusUnauthorized)
	p := newAdapter(t, fake, 1000)
	lister, ok := p.(provider.ModelLister)
	if !ok {
		t.Fatal("messages adapter does not implement provider.ModelLister")
	}
	ids, err := lister.ListModels(context.Background())
	if err == nil {
		t.Fatalf("ListModels() = %v, want non-nil error", ids)
	}
	if !strings.Contains(err.Error(), "list models") {
		t.Fatalf("ListModels() error = %v, want wrapped \"list models\" context", err)
	}
}
