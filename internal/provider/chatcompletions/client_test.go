package chatcompletions_test

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider/chatcompletions"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"

	openai "github.com/sashabaranov/go-openai"
)

func TestChatTurnStreamsTextAndUsage(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Text: "hello world!"}})
	client := newTestClient(fake.URL)

	var deltas []string
	result, err := client.ChatTurn(context.Background(), chatRequest(), func(delta string) {
		deltas = append(deltas, delta)
	})
	if err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}

	if result.Content != "hello world!" {
		t.Errorf("Content = %q, want %q", result.Content, "hello world!")
	}
	if want := []string{"hello ", "world!"}; !reflect.DeepEqual(deltas, want) {
		t.Errorf("deltas = %#v, want %#v", deltas, want)
	}
	if result.Usage == nil {
		t.Fatal("Usage = nil, want populated usage")
	}
	if result.Usage.TotalTokens != 12 {
		t.Errorf("Usage.TotalTokens = %d, want 12", result.Usage.TotalTokens)
	}

	if fake.RequestCount() != 1 {
		t.Fatalf("RequestCount() = %d, want 1", fake.RequestCount())
	}
	request := fake.Request(0)
	if request["stream"] != true {
		t.Errorf("request stream = %#v, want true", request["stream"])
	}
	streamOptions, ok := request["stream_options"].(map[string]any)
	if !ok {
		t.Fatalf("request stream_options = %#v, want object", request["stream_options"])
	}
	if streamOptions["include_usage"] != true {
		t.Errorf("request stream_options.include_usage = %#v, want true", streamOptions["include_usage"])
	}
}

func TestChatTurnReconstructsToolCallsByIndex(t *testing.T) {
	calls := []testutil.FakeToolCall{
		{
			ID:   "call_weather",
			Name: "get_weather",
			Args: `{"city":"Vancouver","units":"metric"}`,
		},
		{
			ID:   "call_time",
			Name: "get_time",
			Args: `{"timezone":"America/Vancouver"}`,
		},
	}
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{ToolCalls: calls}})
	client := newTestClient(fake.URL)

	result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
	if err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}

	if len(result.ToolCalls) != len(calls) {
		t.Fatalf("len(ToolCalls) = %d, want %d", len(result.ToolCalls), len(calls))
	}
	for i, want := range calls {
		got := result.ToolCalls[i]
		if got.ID != want.ID {
			t.Errorf("ToolCalls[%d].ID = %q, want %q", i, got.ID, want.ID)
		}
		if got.Function.Name != want.Name {
			t.Errorf("ToolCalls[%d].Function.Name = %q, want %q", i, got.Function.Name, want.Name)
		}
		if got.Function.Arguments != want.Args {
			t.Errorf("ToolCalls[%d].Function.Arguments = %q, want %q", i, got.Function.Arguments, want.Args)
		}

		var arguments map[string]any
		if err := json.Unmarshal([]byte(got.Function.Arguments), &arguments); err != nil {
			t.Errorf("ToolCalls[%d].Function.Arguments is not valid JSON: %v", i, err)
		}
	}
}

func TestChatTurnAssemblesSingleIndexlessToolCall(t *testing.T) {
	server := newToolCallStream(t, openai.ToolCall{
		ID:   "call_indexless",
		Type: openai.ToolTypeFunction,
		Function: openai.FunctionCall{
			Name:      "get_weather",
			Arguments: `{"city":"Vancouver"}`,
		},
	})
	client := newTestClient(server.URL)

	result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
	if err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}
	if len(result.ToolCalls) != 1 {
		t.Fatalf("len(ToolCalls) = %d, want 1", len(result.ToolCalls))
	}

	got := result.ToolCalls[0]
	if got.ID != "call_indexless" {
		t.Errorf("ToolCalls[0].ID = %q, want %q", got.ID, "call_indexless")
	}
	if got.Function.Name != "get_weather" {
		t.Errorf("ToolCalls[0].Function.Name = %q, want %q", got.Function.Name, "get_weather")
	}
	if got.Function.Arguments != `{"city":"Vancouver"}` {
		t.Errorf("ToolCalls[0].Function.Arguments = %q, want valid reconstructed arguments", got.Function.Arguments)
	}
}

func TestChatTurnDefaultsMissingToolCallTypeToFunction(t *testing.T) {
	index := 0
	server := newToolCallStream(t, openai.ToolCall{
		Index: &index,
		ID:    "call_typeless",
		Function: openai.FunctionCall{
			Name:      "get_time",
			Arguments: `{"timezone":"America/Vancouver"}`,
		},
	})
	client := newTestClient(server.URL)

	result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
	if err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}
	if len(result.ToolCalls) != 1 {
		t.Fatalf("len(ToolCalls) = %d, want 1", len(result.ToolCalls))
	}
	if result.ToolCalls[0].Type != openai.ToolTypeFunction {
		t.Errorf("ToolCalls[0].Type = %q, want %q", result.ToolCalls[0].Type, openai.ToolTypeFunction)
	}
}

func TestChatTurnRetriesRetryableStatusesThenSucceeds(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{
		{Status: 500},
		{Status: 429},
		{Text: "ok"},
	})
	client := newTestClient(fake.URL)

	result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
	if err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}
	if result.Content != "ok" {
		t.Errorf("Content = %q, want %q", result.Content, "ok")
	}
	if fake.RequestCount() != 3 {
		t.Errorf("RequestCount() = %d, want 3", fake.RequestCount())
	}
}

func TestChatTurnReturnsErrorAfterRetriesAreExhausted(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{
		{Status: 500},
		{Status: 500},
		{Status: 500},
		{Status: 500},
	})
	client := newTestClient(fake.URL)

	result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
	if err == nil {
		t.Fatalf("ChatTurn() result = %#v, want non-nil error", result)
	}
	if fake.RequestCount() != 4 {
		t.Errorf("RequestCount() = %d, want 4", fake.RequestCount())
	}
}

func TestChatTurnDoesNotRetryNonRetryableStatus(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Status: 400}})
	client := newTestClient(fake.URL)

	result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
	if err == nil {
		t.Fatalf("ChatTurn() result = %#v, want non-nil error", result)
	}
	if fake.RequestCount() != 1 {
		t.Errorf("RequestCount() = %d, want 1", fake.RequestCount())
	}
}

func TestChatTurnStopsBackoffWhenContextIsCanceled(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Status: 500}})
	client := chatcompletions.NewClient("test-key", fake.URL)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	client.Backoff = func(int) time.Duration {
		time.AfterFunc(50*time.Millisecond, cancel)
		return time.Hour
	}

	type outcome struct {
		result *chatcompletions.TurnResult
		err    error
	}
	resultCh := make(chan outcome, 1)
	started := time.Now()
	go func() {
		result, err := client.ChatTurn(ctx, chatRequest(), func(string) {})
		resultCh <- outcome{result: result, err: err}
	}()

	select {
	case got := <-resultCh:
		if !errors.Is(got.err, context.Canceled) {
			t.Fatalf("ChatTurn() = (%#v, %v), want context.Canceled", got.result, got.err)
		}
		if elapsed := time.Since(started); elapsed >= time.Second {
			t.Errorf("ChatTurn() took %v, want cancellation well before the one-hour backoff", elapsed)
		}
	case <-time.After(time.Second):
		t.Fatal("ChatTurn() did not stop promptly when context was canceled during backoff")
	}
	if fake.RequestCount() != 1 {
		t.Errorf("RequestCount() = %d, want 1", fake.RequestCount())
	}
}

func newTestClient(baseURL string) *chatcompletions.Client {
	client := chatcompletions.NewClient("test-key", baseURL)
	client.Backoff = func(int) time.Duration { return 0 }

	return client
}

func chatRequest() openai.ChatCompletionRequest {
	return openai.ChatCompletionRequest{
		Model: "deepseek-chat",
		Messages: []openai.ChatCompletionMessage{{
			Role:    openai.ChatMessageRoleUser,
			Content: "hello",
		}},
	}
}

func newToolCallStream(t *testing.T, call openai.ToolCall) *httptest.Server {
	t.Helper()

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/chat/completions" {
			http.Error(w, "unexpected request", http.StatusNotFound)
			return
		}

		payload, err := json.Marshal(openai.ChatCompletionStreamResponse{
			Choices: []openai.ChatCompletionStreamChoice{{
				Delta: openai.ChatCompletionStreamChoiceDelta{
					ToolCalls: []openai.ToolCall{call},
				},
			}},
		})
		if err != nil {
			t.Errorf("marshal stream response: %v", err)
			return
		}
		finishPayload, err := json.Marshal(finishChunk(openai.FinishReasonToolCalls))
		if err != nil {
			t.Errorf("marshal finish chunk: %v", err)
			return
		}

		w.Header().Set("Content-Type", "text/event-stream")
		if _, err := fmt.Fprintf(w, "data: %s\n\ndata: %s\n\ndata: [DONE]\n\n", payload, finishPayload); err != nil {
			t.Errorf("write stream response: %v", err)
		}
	}))
	t.Cleanup(server.Close)

	return server
}

func TestChatTurnCapturesReasoningSeparately(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{Reasoning: "think first", Text: "answer"}})
	client := newTestClient(fake.URL)

	result, err := client.ChatTurn(context.Background(), openai.ChatCompletionRequest{
		Model:    "deepseek-v4-pro",
		Messages: []openai.ChatCompletionMessage{{Role: openai.ChatMessageRoleUser, Content: "hi"}},
	}, nil)
	if err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}
	if result.Reasoning != "think first" || result.Content != "answer" {
		t.Fatalf("ChatTurn() = %#v", result)
	}
}

// doneSentinel terminates a well-formed stream.
const doneSentinel = "data: [DONE]\n\n"

// sseBody renders chunks as the exact SSE body of one scripted response. The
// [DONE] sentinel is not appended, so a test can also script a body that ends
// without it.
func sseBody(t *testing.T, chunks ...openai.ChatCompletionStreamResponse) string {
	t.Helper()

	var body strings.Builder
	for _, chunk := range chunks {
		payload, err := json.Marshal(chunk)
		if err != nil {
			t.Fatalf("marshal stream chunk: %v", err)
		}
		fmt.Fprintf(&body, "data: %s\n\n", payload)
	}

	return body.String()
}

func deltaChunk(delta openai.ChatCompletionStreamChoiceDelta) openai.ChatCompletionStreamResponse {
	return openai.ChatCompletionStreamResponse{
		Choices: []openai.ChatCompletionStreamChoice{{Delta: delta}},
	}
}

// finishChunk sends the finish reason on an otherwise-empty delta, the way
// OpenAI-compatible servers do.
func finishChunk(reason openai.FinishReason) openai.ChatCompletionStreamResponse {
	return openai.ChatCompletionStreamResponse{
		Choices: []openai.ChatCompletionStreamChoice{{FinishReason: reason}},
	}
}

func usageChunk() openai.ChatCompletionStreamResponse {
	return openai.ChatCompletionStreamResponse{
		Choices: []openai.ChatCompletionStreamChoice{},
		Usage:   &openai.Usage{PromptTokens: 7, CompletionTokens: 5, TotalTokens: 12},
	}
}

func intPtr(v int) *int { return &v }

// TestChatTurnRejectsUnfinishedStreams proves that truncated or filtered
// answers and truncated transports come back as errors, not as successful
// turns: finish_reason "length" and "content_filter" name themselves, and a
// body that ends without any finish reason (with or without [DONE]) is a
// truncated stream.
func TestChatTurnRejectsUnfinishedStreams(t *testing.T) {
	midToolCall := openai.ToolCall{
		ID:   "call_write",
		Type: openai.ToolTypeFunction,
		Function: openai.FunctionCall{
			Name:      "write_file",
			Arguments: `{"path":"a.go","content":"package ma`,
		},
	}

	tests := []struct {
		name    string
		body    string
		wantErr string
	}{
		{
			name: "finish_reason length after text",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{Content: "The fix is to chan"}),
				finishChunk(openai.FinishReasonLength),
				usageChunk(),
			) + doneSentinel,
			wantErr: `finish_reason "length"`,
		},
		{
			name: "finish_reason length mid tool call",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{midToolCall}}),
				finishChunk(openai.FinishReasonLength),
				usageChunk(),
			) + doneSentinel,
			wantErr: `finish_reason "length"`,
		},
		{
			name: "finish_reason content_filter after text",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{Content: "Sure, here"}),
				finishChunk(openai.FinishReasonContentFilter),
				usageChunk(),
			) + doneSentinel,
			wantErr: `finish_reason "content_filter"`,
		},
		{
			name:    "body ends without finish reason or DONE",
			body:    sseBody(t, deltaChunk(openai.ChatCompletionStreamChoiceDelta{Content: "half an ans"})),
			wantErr: "without a finish reason",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{SSE: tt.body}})
			client := newTestClient(fake.URL)

			result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
			if err == nil {
				t.Fatalf("ChatTurn() = %#v, want an error containing %q", result, tt.wantErr)
			}
			if !strings.Contains(err.Error(), tt.wantErr) {
				t.Errorf("ChatTurn() error = %q, want it to contain %q", err, tt.wantErr)
			}
		})
	}
}

// TestChatTurnAssemblesToolCallFragments proves fragments become the calls
// the server sent: index-less calls stay separate, an index-only first
// fragment gets its id and name from the later fragment, and a call the
// stream never identifies fails the turn instead of leaving an empty
// tool_call_id behind.
func TestChatTurnAssemblesToolCallFragments(t *testing.T) {
	type wantCall struct{ id, name, args string }

	tests := []struct {
		name    string
		body    string
		want    []wantCall
		wantErr string
	}{
		{
			name: "two index-less calls in separate chunks",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					ID:       "call_ls",
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: "shell", Arguments: `{"command":"ls"}`},
				}}}),
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					ID:       "call_read",
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: "read_file", Arguments: `{"path":"x.txt"}`},
				}}}),
				finishChunk(openai.FinishReasonToolCalls),
				usageChunk(),
			) + doneSentinel,
			want: []wantCall{
				{id: "call_ls", name: "shell", args: `{"command":"ls"}`},
				{id: "call_read", name: "read_file", args: `{"path":"x.txt"}`},
			},
		},
		{
			name: "index-only first fragment filled by the id and name fragment",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					Index: intPtr(0),
				}}}),
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					ID:       "call_weather",
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: "get_weather", Arguments: `{"city":"Vancouver"}`},
				}}}),
				finishChunk(openai.FinishReasonToolCalls),
				usageChunk(),
			) + doneSentinel,
			want: []wantCall{{id: "call_weather", name: "get_weather", args: `{"city":"Vancouver"}`}},
		},
		{
			name: "index-less continuation appends arguments",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					ID:       "call_shell",
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: "shell", Arguments: `{"comm`},
				}}}),
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					Function: openai.FunctionCall{Arguments: `and":"ls"}`},
				}}}),
				finishChunk(openai.FinishReasonToolCalls),
				usageChunk(),
			) + doneSentinel,
			want: []wantCall{{id: "call_shell", name: "shell", args: `{"command":"ls"}`}},
		},
		{
			name: "call without a name fails",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					ID:       "call_anon",
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Arguments: `{"city":"Vancouver"}`},
				}}}),
				finishChunk(openai.FinishReasonToolCalls),
				usageChunk(),
			) + doneSentinel,
			wantErr: "no name",
		},
		{
			name: "call without an id fails",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: "shell", Arguments: `{"command":"ls"}`},
				}}}),
				finishChunk(openai.FinishReasonToolCalls),
				usageChunk(),
			) + doneSentinel,
			wantErr: "has no id",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{SSE: tt.body}})
			client := newTestClient(fake.URL)

			result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
			if tt.wantErr != "" {
				if err == nil {
					t.Fatalf("ChatTurn() = %#v, want an error containing %q", result, tt.wantErr)
				}
				if !strings.Contains(err.Error(), tt.wantErr) {
					t.Errorf("ChatTurn() error = %q, want it to contain %q", err, tt.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("ChatTurn() error = %v", err)
			}
			if len(result.ToolCalls) != len(tt.want) {
				t.Fatalf("len(ToolCalls) = %d, want %d (%#v)", len(result.ToolCalls), len(tt.want), result.ToolCalls)
			}
			for i, want := range tt.want {
				got := result.ToolCalls[i]
				if got.ID != want.id || got.Function.Name != want.name || got.Function.Arguments != want.args {
					t.Errorf("ToolCalls[%d] = %#v, want id=%q name=%q args=%q", i, got, want.id, want.name, want.args)
				}
			}
		})
	}
}

// TestChatTurnAcceptsWellFormedStreams pins the streams that must keep
// succeeding: a DeepSeek-style text stream ending in stop with a usage chunk
// after the finish chunk, an indexed tool-call stream ending in tool_calls,
// and a reasoning_content stream ending in stop.
func TestChatTurnAcceptsWellFormedStreams(t *testing.T) {
	type wantCall struct{ id, name, args string }

	tests := []struct {
		name          string
		body          string
		wantText      string
		wantReasoning string
		wantCalls     []wantCall
	}{
		{
			name: "deepseek text stream ending in stop with usage chunk",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{Content: "Let me "}),
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{Content: "check."}),
				finishChunk(openai.FinishReasonStop),
				usageChunk(),
			) + doneSentinel,
			wantText: "Let me check.",
		},
		{
			name: "indexed tool call stream ending in tool_calls",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					Index:    intPtr(0),
					ID:       "call_a",
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: "shell", Arguments: `{"comm`},
				}}}),
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					Index:    intPtr(0),
					Function: openai.FunctionCall{Arguments: `and":"ls"}`},
				}}}),
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ToolCalls: []openai.ToolCall{{
					Index:    intPtr(1),
					ID:       "call_b",
					Type:     openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: "get_weather", Arguments: `{"city":"Vancouver"}`},
				}}}),
				finishChunk(openai.FinishReasonToolCalls),
				usageChunk(),
			) + doneSentinel,
			wantCalls: []wantCall{
				{id: "call_a", name: "shell", args: `{"command":"ls"}`},
				{id: "call_b", name: "get_weather", args: `{"city":"Vancouver"}`},
			},
		},
		{
			name: "reasoning content stream ending in stop",
			body: sseBody(t,
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{ReasoningContent: "think first"}),
				deltaChunk(openai.ChatCompletionStreamChoiceDelta{Content: "answer"}),
				finishChunk(openai.FinishReasonStop),
				usageChunk(),
			) + doneSentinel,
			wantText:      "answer",
			wantReasoning: "think first",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{SSE: tt.body}})
			client := newTestClient(fake.URL)

			result, err := client.ChatTurn(context.Background(), chatRequest(), func(string) {})
			if err != nil {
				t.Fatalf("ChatTurn() error = %v", err)
			}
			if result.Content != tt.wantText {
				t.Errorf("Content = %q, want %q", result.Content, tt.wantText)
			}
			if result.Reasoning != tt.wantReasoning {
				t.Errorf("Reasoning = %q, want %q", result.Reasoning, tt.wantReasoning)
			}
			if result.Usage == nil || result.Usage.TotalTokens != 12 {
				t.Errorf("Usage = %#v, want TotalTokens 12", result.Usage)
			}
			if len(result.ToolCalls) != len(tt.wantCalls) {
				t.Fatalf("len(ToolCalls) = %d, want %d (%#v)", len(result.ToolCalls), len(tt.wantCalls), result.ToolCalls)
			}
			for i, want := range tt.wantCalls {
				got := result.ToolCalls[i]
				if got.ID != want.id || got.Function.Name != want.name || got.Function.Arguments != want.args {
					t.Errorf("ToolCalls[%d] = %#v, want id=%q name=%q args=%q", i, got, want.id, want.name, want.args)
				}
			}
		})
	}
}
