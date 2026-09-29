package testutil

// ponytail: Go's default same-package coverage attribution makes this shared test
// infrastructure read near-zero because internal/provider/chatcompletions and internal/agent tests
// exercise it. Use -coverpkg=./... for a direct read, or exclude internal/testutil
// from the aggregate when enforcing a same-package coverage threshold.

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"

	openai "github.com/sashabaranov/go-openai"
)

type FakeToolCall struct {
	ID   string
	Name string
	Args string
}

// FakeTurn scripts either a text reply or a tool-call batch. Text and
// ToolCalls are mutually exclusive.
type FakeTurn struct {
	Status    int
	Reasoning string
	Text      string
	ToolCalls []FakeToolCall
}

type FakeChat struct {
	*httptest.Server

	t            testing.TB
	mu           sync.Mutex
	turns        []FakeTurn
	requests     []map[string]any
	headers      []http.Header
	models       []string
	modelsStatus int
}

func NewFakeChat(t testing.TB, turns []FakeTurn) *FakeChat {
	t.Helper()
	for i, turn := range turns {
		if turn.Text != "" && len(turn.ToolCalls) > 0 {
			t.Fatalf("FakeTurn %d: Text and ToolCalls are mutually exclusive", i)
		}
	}

	fake := &FakeChat{
		t:     t,
		turns: append([]FakeTurn(nil), turns...),
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/chat/completions", fake.handleChatCompletions)
	mux.HandleFunc("/models", fake.handleModels)
	fake.Server = httptest.NewServer(mux)
	t.Cleanup(fake.Close)

	return fake
}

// SetModels configures the ids served by GET /models. Default: none.
func (f *FakeChat) SetModels(ids []string) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.models = append([]string(nil), ids...)
}

// SetModelsStatus scripts a non-200 HTTP status for GET /models, independent of
// the turn scripting, so tests can assert ListModels surfaces HTTP errors.
func (f *FakeChat) SetModelsStatus(status int) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.modelsStatus = status
}

func (f *FakeChat) RequestCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()

	return len(f.requests)
}

func (f *FakeChat) Request(i int) map[string]any {
	f.t.Helper()

	f.mu.Lock()
	defer f.mu.Unlock()

	if i < 0 || i >= len(f.requests) {
		f.t.Fatalf("request index %d out of range; recorded %d requests", i, len(f.requests))
	}

	return f.requests[i]
}

// RequestHeaderCount returns the number of requests received on any endpoint.
func (f *FakeChat) RequestHeaderCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()

	return len(f.headers)
}

// RequestHeaders returns a snapshot of the HTTP headers of the i-th request
// the server received on any endpoint (turns and model listings alike).
func (f *FakeChat) RequestHeaders(i int) http.Header {
	f.t.Helper()

	f.mu.Lock()
	defer f.mu.Unlock()

	if i < 0 || i >= len(f.headers) {
		f.t.Fatalf("request header index %d out of range; recorded %d requests", i, len(f.headers))
	}

	return f.headers[i]
}

func (f *FakeChat) handleChatCompletions(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	var request map[string]any
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		f.t.Errorf("decode chat completion request: %v", err)
		http.Error(w, "invalid JSON request", http.StatusBadRequest)
		return
	}

	f.mu.Lock()
	f.requests = append(f.requests, request)
	f.headers = append(f.headers, r.Header.Clone())
	if len(f.turns) == 0 {
		f.mu.Unlock()
		f.t.Errorf("unexpected extra request")
		http.Error(w, `{"error":{"message":"unexpected extra request","type":"server_error"}}`, http.StatusInternalServerError)
		return
	}
	turn := f.turns[0]
	f.turns = f.turns[1:]
	f.mu.Unlock()

	if turn.Status != 0 && turn.Status != http.StatusOK {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(turn.Status)
		_, _ = io.WriteString(w, `{"error":{"message":"scripted failure","type":"server_error"}}`)
		return
	}

	flusher, ok := w.(http.Flusher)
	if !ok {
		f.t.Errorf("response writer does not support streaming")
		http.Error(w, "streaming unsupported", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")

	if turn.Reasoning != "" {
		if !f.writeChunk(w, flusher, openai.ChatCompletionStreamResponse{
			Choices: []openai.ChatCompletionStreamChoice{{
				Delta: openai.ChatCompletionStreamChoiceDelta{ReasoningContent: turn.Reasoning},
			}},
		}) {
			return
		}
	}

	if len(turn.ToolCalls) == 0 {
		first, second := splitInHalf(turn.Text)
		for _, content := range []string{first, second} {
			if !f.writeChunk(w, flusher, openai.ChatCompletionStreamResponse{
				Choices: []openai.ChatCompletionStreamChoice{{
					Delta: openai.ChatCompletionStreamChoiceDelta{Content: content},
				}},
			}) {
				return
			}
		}
	} else {
		for i, call := range turn.ToolCalls {
			first, second := splitInHalf(call.Args)
			index := i
			if !f.writeChunk(w, flusher, openai.ChatCompletionStreamResponse{
				Choices: []openai.ChatCompletionStreamChoice{{
					Delta: openai.ChatCompletionStreamChoiceDelta{
						ToolCalls: []openai.ToolCall{{
							Index: &index,
							ID:    call.ID,
							Type:  openai.ToolTypeFunction,
							Function: openai.FunctionCall{
								Name:      call.Name,
								Arguments: first,
							},
						}},
					},
				}},
			}) {
				return
			}

			if !f.writeChunk(w, flusher, openai.ChatCompletionStreamResponse{
				Choices: []openai.ChatCompletionStreamChoice{{
					Delta: openai.ChatCompletionStreamChoiceDelta{
						ToolCalls: []openai.ToolCall{{
							Index: &index,
							Function: openai.FunctionCall{
								Arguments: second,
							},
						}},
					},
				}},
			}) {
				return
			}
		}

		if !f.writeChunk(w, flusher, openai.ChatCompletionStreamResponse{
			Choices: []openai.ChatCompletionStreamChoice{{
				FinishReason: openai.FinishReasonToolCalls,
			}},
		}) {
			return
		}
	}

	if !f.writeChunk(w, flusher, openai.ChatCompletionStreamResponse{
		Choices: []openai.ChatCompletionStreamChoice{},
		Usage: &openai.Usage{
			PromptTokens:     7,
			CompletionTokens: 5,
			TotalTokens:      12,
		},
	}) {
		return
	}

	if _, err := io.WriteString(w, "data: [DONE]\n\n"); err != nil {
		f.t.Errorf("write end-of-stream marker: %v", err)
		return
	}
	flusher.Flush()
}

func (f *FakeChat) handleModels(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	f.mu.Lock()
	f.headers = append(f.headers, r.Header.Clone())
	status := f.modelsStatus
	ids := append([]string(nil), f.models...)
	f.mu.Unlock()

	w.Header().Set("Content-Type", "application/json")
	if status != 0 && status != http.StatusOK {
		w.WriteHeader(status)
		_, _ = io.WriteString(w, `{"error":{"message":"scripted failure","type":"server_error"}}`)
		return
	}

	data := make([]map[string]string, 0, len(ids))
	for _, id := range ids {
		data = append(data, map[string]string{"id": id, "object": "model"})
	}
	if err := json.NewEncoder(w).Encode(map[string]any{"object": "list", "data": data}); err != nil {
		f.t.Errorf("encode models response: %v", err)
	}
}

func (f *FakeChat) writeChunk(
	w http.ResponseWriter,
	flusher http.Flusher,
	response openai.ChatCompletionStreamResponse,
) bool {
	payload, err := json.Marshal(response)
	if err != nil {
		f.t.Errorf("marshal chat completion chunk: %v", err)
		return false
	}

	if _, err := fmt.Fprintf(w, "data: %s\n\n", payload); err != nil {
		f.t.Errorf("write chat completion chunk: %v", err)
		return false
	}
	flusher.Flush()

	return true
}

func splitInHalf(value string) (string, string) {
	runes := []rune(value)
	middle := len(runes) / 2

	return string(runes[:middle]), string(runes[middle:])
}
