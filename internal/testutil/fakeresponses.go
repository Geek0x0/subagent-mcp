package testutil

// ponytail: Go's default same-package coverage attribution makes this shared test
// infrastructure read near-zero because internal/provider/responses tests
// exercise it. Use -coverpkg=./... for a direct read, or exclude internal/testutil
// from the aggregate when enforcing a same-package coverage threshold.

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"sync"
	"sync/atomic"
	"testing"
)

// FakeResponseItem is one output item of a scripted Responses API turn.
// Exactly one group of fields is set per item.
type FakeResponseItem struct {
	// ReasoningSummary and EncryptedContent form a reasoning item.
	ReasoningSummary string
	EncryptedContent string
	// MessageText is an assistant message with one output_text part.
	MessageText string
	// MessageRefusal is an assistant message with one refusal part.
	MessageRefusal string
	// FunctionCallID, FunctionName and FunctionArguments form a function_call item.
	FunctionCallID    string
	FunctionName      string
	FunctionArguments string
}

// FakeResponse scripts one POST /responses reply.
type FakeResponse struct {
	Status           int // non-zero and non-200: JSON error body with this HTTP status
	Items            []FakeResponseItem
	ResponseStatus   string // "completed" (default), "incomplete", "failed"
	IncompleteReason string
	InputTokens      int
	CachedTokens     int
	OutputTokens     int
	ReasoningTokens  int
	// StreamError, when set, emits an in-stream `error` event with this code
	// and message instead of a terminal response event.
	StreamError *FakeResponseStreamError
}

// FakeResponseStreamError is the payload of an in-stream `error` event.
type FakeResponseStreamError struct {
	Code    string
	Message string
}

// FakeResponses is an httptest server speaking the OpenAI Responses SSE
// protocol, mirroring FakeChat: it records decoded request bodies and fails the
// test on unexpected extra requests.
type FakeResponses struct {
	*httptest.Server

	t            testing.TB
	mu           sync.Mutex
	conns        atomic.Int64
	responses    []FakeResponse
	requests     []map[string]any
	headers      []http.Header
	models       []string
	modelsStatus int
}

// NewFakeResponses starts a fake Responses API server scripted with responses.
func NewFakeResponses(t testing.TB, responses []FakeResponse) *FakeResponses {
	t.Helper()
	for i, response := range responses {
		for j, item := range response.Items {
			if groups := itemGroupCount(item); groups != 1 {
				t.Fatalf("FakeResponse %d item %d: exactly one of reasoning, message, or function call must be set, got %d groups", i, j, groups)
			}
		}
	}

	fake := &FakeResponses{
		t:         t,
		responses: append([]FakeResponse(nil), responses...),
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/responses", fake.handleResponses)
	mux.HandleFunc("/models", fake.handleModels)
	fake.Server = newConnCountingServer(mux, &fake.conns)
	t.Cleanup(fake.Close)

	return fake
}

// SetModels configures the ids served by GET /models. Default: none.
func (f *FakeResponses) SetModels(ids []string) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.models = append([]string(nil), ids...)
}

// SetModelsStatus scripts a non-200 HTTP status for GET /models, independent of
// the response scripting, so tests can assert ListModels surfaces HTTP errors.
func (f *FakeResponses) SetModelsStatus(status int) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.modelsStatus = status
}

// RequestCount returns the number of requests recorded so far.
func (f *FakeResponses) RequestCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()

	return len(f.requests)
}

// Request returns the i-th recorded decoded request body.
func (f *FakeResponses) Request(i int) map[string]any {
	f.t.Helper()

	f.mu.Lock()
	defer f.mu.Unlock()

	if i < 0 || i >= len(f.requests) {
		f.t.Fatalf("request index %d out of range; recorded %d requests", i, len(f.requests))
	}

	return f.requests[i]
}

// RequestHeaderCount returns the number of requests received on any endpoint.
func (f *FakeResponses) RequestHeaderCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()

	return len(f.headers)
}

// ConnectionCount returns the number of TCP connections the server accepted.
func (f *FakeResponses) ConnectionCount() int64 {
	return f.conns.Load()
}

// RequestHeaders returns a snapshot of the HTTP headers of the i-th request
// the server received on any endpoint (turns and model listings alike).
func (f *FakeResponses) RequestHeaders(i int) http.Header {
	f.t.Helper()

	f.mu.Lock()
	defer f.mu.Unlock()

	if i < 0 || i >= len(f.headers) {
		f.t.Fatalf("request header index %d out of range; recorded %d requests", i, len(f.headers))
	}

	return f.headers[i]
}

func (f *FakeResponses) handleResponses(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	var request map[string]any
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		f.t.Errorf("decode responses request: %v", err)
		http.Error(w, "invalid JSON request", http.StatusBadRequest)
		return
	}

	f.mu.Lock()
	f.requests = append(f.requests, request)
	f.headers = append(f.headers, r.Header.Clone())
	requestIndex := len(f.requests) - 1
	if len(f.responses) == 0 {
		f.mu.Unlock()
		f.t.Errorf("unexpected extra request")
		http.Error(w, `{"error":{"message":"unexpected extra request","type":"server_error"}}`, http.StatusInternalServerError)
		return
	}
	scripted := f.responses[0]
	f.responses = f.responses[1:]
	f.mu.Unlock()

	if scripted.Status != 0 && scripted.Status != http.StatusOK {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(scripted.Status)
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

	sequence := 0
	writeEvent := func(eventType string, payload map[string]any) bool {
		payload["type"] = eventType
		data, err := json.Marshal(payload)
		if err != nil {
			f.t.Errorf("marshal %s event: %v", eventType, err)
			return false
		}
		if _, err := fmt.Fprintf(w, "event: %s\ndata: %s\n\n", eventType, data); err != nil {
			f.t.Errorf("write %s event: %v", eventType, err)
			return false
		}
		flusher.Flush()
		return true
	}

	responseID := fmt.Sprintf("resp_%d", requestIndex)
	model, _ := request["model"].(string)

	created := map[string]any{
		"id":         responseID,
		"object":     "response",
		"created_at": 1700000000,
		"status":     "in_progress",
		"model":      model,
		"output":     []any{},
	}
	if !writeEvent("response.created", map[string]any{"sequence_number": sequence, "response": created}) {
		return
	}
	sequence++

	output := make([]any, 0, len(scripted.Items))
	for i, item := range scripted.Items {
		if item.MessageText != "" {
			if !writeEvent("response.output_text.delta", map[string]any{
				"sequence_number": sequence,
				"item_id":         fmt.Sprintf("msg_%d", i),
				"output_index":    i,
				"content_index":   0,
				"delta":           item.MessageText,
			}) {
				return
			}
			sequence++
		}
		output = append(output, renderFakeResponseItem(item, i))
	}

	if scripted.StreamError != nil {
		writeEvent("error", map[string]any{
			"sequence_number": sequence,
			"code":            scripted.StreamError.Code,
			"message":         scripted.StreamError.Message,
		})
		return
	}

	status := scripted.ResponseStatus
	if status == "" {
		status = "completed"
	}
	response := map[string]any{
		"id":         responseID,
		"object":     "response",
		"created_at": 1700000000,
		"status":     status,
		"model":      model,
		"output":     output,
		"usage": map[string]any{
			"input_tokens":          scripted.InputTokens,
			"input_tokens_details":  map[string]any{"cached_tokens": scripted.CachedTokens},
			"output_tokens":         scripted.OutputTokens,
			"output_tokens_details": map[string]any{"reasoning_tokens": scripted.ReasoningTokens},
			"total_tokens":          scripted.InputTokens + scripted.OutputTokens,
		},
	}
	switch status {
	case "incomplete":
		response["incomplete_details"] = map[string]any{"reason": scripted.IncompleteReason}
	case "failed":
		response["error"] = map[string]any{"code": "server_error", "message": "scripted failure"}
	}
	writeEvent("response."+status, map[string]any{"sequence_number": sequence, "response": response})
}

func (f *FakeResponses) handleModels(w http.ResponseWriter, r *http.Request) {
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

func itemGroupCount(item FakeResponseItem) int {
	groups := 0
	if item.ReasoningSummary != "" || item.EncryptedContent != "" {
		groups++
	}
	if item.MessageText != "" || item.MessageRefusal != "" {
		groups++
	}
	if item.FunctionCallID != "" || item.FunctionName != "" || item.FunctionArguments != "" {
		groups++
	}
	return groups
}

func renderFakeResponseItem(item FakeResponseItem, index int) map[string]any {
	switch {
	case item.ReasoningSummary != "" || item.EncryptedContent != "":
		return map[string]any{
			"type": "reasoning",
			"id":   fmt.Sprintf("rs_%d", index),
			"summary": []any{map[string]any{
				"type": "summary_text",
				"text": item.ReasoningSummary,
			}},
			"encrypted_content": item.EncryptedContent,
		}
	case item.MessageText != "" || item.MessageRefusal != "":
		content := []any{}
		if item.MessageText != "" {
			content = append(content, map[string]any{
				"type":        "output_text",
				"text":        item.MessageText,
				"annotations": []any{},
			})
		} else {
			content = append(content, map[string]any{
				"type":    "refusal",
				"refusal": item.MessageRefusal,
			})
		}
		return map[string]any{
			"type":    "message",
			"id":      fmt.Sprintf("msg_%d", index),
			"role":    "assistant",
			"status":  "completed",
			"content": content,
		}
	default:
		return map[string]any{
			"type":      "function_call",
			"id":        fmt.Sprintf("fc_%d", index),
			"call_id":   item.FunctionCallID,
			"name":      item.FunctionName,
			"arguments": item.FunctionArguments,
			"status":    "completed",
		}
	}
}
