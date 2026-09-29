package testutil

// ponytail: Go's default same-package coverage attribution makes this shared test
// infrastructure read near-zero because internal/provider/messages tests exercise
// it. Use -coverpkg=./... for a direct read, or exclude internal/testutil from the
// aggregate when enforcing a same-package coverage threshold.

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
)

// FakeBlock is one content block of a scripted Messages API turn.
// Exactly one group of fields is set per block.
type FakeBlock struct {
	// Thinking and Signature form a thinking block:
	// a thinking_delta followed by a signature_delta.
	Thinking  string
	Signature string
	// Text is a text block streamed as one text_delta.
	Text string
	// ToolID, ToolName and ToolInputJSON form a tool_use block whose input is
	// streamed verbatim as one input_json_delta. ToolInputChunks, when set,
	// replaces that single delta with one delta per chunk; when both input
	// fields are empty no input_json_delta is emitted for the block.
	ToolID          string
	ToolName        string
	ToolInputJSON   string
	ToolInputChunks []string
}

// FakeMessage scripts one POST /v1/messages reply.
type FakeMessage struct {
	Status int // non-zero and non-200: JSON error body with this HTTP status
	Blocks []FakeBlock
	// StopReason defaults to "tool_use" when any tool block is present and
	// "end_turn" otherwise.
	StopReason string
	// RefusalCategory is sent in stop_details when StopReason is "refusal".
	RefusalCategory     string
	InputTokens         int
	CacheReadTokens     int
	CacheCreationTokens int
	OutputTokens        int
}

// FakeMessages is an httptest server speaking the Anthropic Messages SSE
// protocol, mirroring FakeChat: it records decoded request bodies and fails the
// test on unexpected extra requests.
type FakeMessages struct {
	*httptest.Server

	t            testing.TB
	mu           sync.Mutex
	messages     []FakeMessage
	requests     []map[string]any
	headers      []http.Header
	models       []string
	modelsStatus int
}

// NewFakeMessages starts a fake Messages API server scripted with messages.
func NewFakeMessages(t testing.TB, messages []FakeMessage) *FakeMessages {
	t.Helper()
	for i, message := range messages {
		for j, block := range message.Blocks {
			if groups := blockGroupCount(block); groups != 1 {
				t.Fatalf("FakeMessage %d block %d: exactly one of thinking, text, or tool use must be set, got %d groups", i, j, groups)
			}
		}
	}

	fake := &FakeMessages{
		t:        t,
		messages: append([]FakeMessage(nil), messages...),
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/v1/messages", fake.handleMessages)
	mux.HandleFunc("/v1/models", fake.handleModels)
	fake.Server = httptest.NewServer(mux)
	t.Cleanup(fake.Close)

	return fake
}

// SetModels configures the ids served by GET /v1/models. Default: none.
func (f *FakeMessages) SetModels(ids []string) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.models = append([]string(nil), ids...)
}

// SetModelsStatus scripts a non-200 HTTP status for GET /v1/models, independent
// of the message scripting, so tests can assert ListModels surfaces HTTP errors.
func (f *FakeMessages) SetModelsStatus(status int) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.modelsStatus = status
}

// RequestCount returns the number of requests recorded so far.
func (f *FakeMessages) RequestCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()

	return len(f.requests)
}

// Request returns the i-th recorded decoded request body.
func (f *FakeMessages) Request(i int) map[string]any {
	f.t.Helper()

	f.mu.Lock()
	defer f.mu.Unlock()

	if i < 0 || i >= len(f.requests) {
		f.t.Fatalf("request index %d out of range; recorded %d requests", i, len(f.requests))
	}

	return f.requests[i]
}

// RequestHeaderCount returns the number of requests received on any endpoint.
func (f *FakeMessages) RequestHeaderCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()

	return len(f.headers)
}

// RequestHeaders returns a snapshot of the HTTP headers of the i-th request
// the server received on any endpoint (turns and model listings alike).
func (f *FakeMessages) RequestHeaders(i int) http.Header {
	f.t.Helper()

	f.mu.Lock()
	defer f.mu.Unlock()

	if i < 0 || i >= len(f.headers) {
		f.t.Fatalf("request header index %d out of range; recorded %d requests", i, len(f.headers))
	}

	return f.headers[i]
}

func (f *FakeMessages) handleMessages(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	var request map[string]any
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		f.t.Errorf("decode messages request: %v", err)
		http.Error(w, "invalid JSON request", http.StatusBadRequest)
		return
	}

	f.mu.Lock()
	f.requests = append(f.requests, request)
	f.headers = append(f.headers, r.Header.Clone())
	requestIndex := len(f.requests) - 1
	if len(f.messages) == 0 {
		f.mu.Unlock()
		f.t.Errorf("unexpected extra request")
		http.Error(w, `{"type":"error","error":{"type":"server_error","message":"unexpected extra request"}}`, http.StatusInternalServerError)
		return
	}
	scripted := f.messages[0]
	f.messages = f.messages[1:]
	f.mu.Unlock()

	if scripted.Status != 0 && scripted.Status != http.StatusOK {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(scripted.Status)
		_, _ = io.WriteString(w, `{"type":"error","error":{"type":"invalid_request_error","message":"scripted failure"}}`)
		return
	}

	flusher, ok := w.(http.Flusher)
	if !ok {
		f.t.Errorf("response writer does not support streaming")
		http.Error(w, "streaming unsupported", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")

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

	model, _ := request["model"].(string)
	messageID := fmt.Sprintf("msg_%d", requestIndex)

	if !writeEvent("message_start", map[string]any{
		"message": map[string]any{
			"id":            messageID,
			"type":          "message",
			"role":          "assistant",
			"model":         model,
			"content":       []any{},
			"stop_reason":   nil,
			"stop_sequence": nil,
			"usage": map[string]any{
				"input_tokens":                scripted.InputTokens,
				"cache_read_input_tokens":     scripted.CacheReadTokens,
				"cache_creation_input_tokens": scripted.CacheCreationTokens,
				"output_tokens":               0,
			},
		},
	}) {
		return
	}

	hasTool := false
	for index, block := range scripted.Blocks {
		switch {
		case block.Thinking != "" || block.Signature != "":
			if !writeEvent("content_block_start", map[string]any{
				"index":         index,
				"content_block": map[string]any{"type": "thinking", "thinking": "", "signature": ""},
			}) {
				return
			}
			if block.Thinking != "" && !writeEvent("content_block_delta", map[string]any{
				"index": index,
				"delta": map[string]any{"type": "thinking_delta", "thinking": block.Thinking},
			}) {
				return
			}
			if !writeEvent("content_block_delta", map[string]any{
				"index": index,
				"delta": map[string]any{"type": "signature_delta", "signature": block.Signature},
			}) {
				return
			}
		case block.Text != "":
			if !writeEvent("content_block_start", map[string]any{
				"index":         index,
				"content_block": map[string]any{"type": "text", "text": ""},
			}) {
				return
			}
			if !writeEvent("content_block_delta", map[string]any{
				"index": index,
				"delta": map[string]any{"type": "text_delta", "text": block.Text},
			}) {
				return
			}
		default:
			hasTool = true
			if !writeEvent("content_block_start", map[string]any{
				"index": index,
				"content_block": map[string]any{
					"type":  "tool_use",
					"id":    block.ToolID,
					"name":  block.ToolName,
					"input": map[string]any{},
				},
			}) {
				return
			}
			chunks := block.ToolInputChunks
			if len(chunks) == 0 && block.ToolInputJSON != "" {
				chunks = []string{block.ToolInputJSON}
			}
			for _, chunk := range chunks {
				if !writeEvent("content_block_delta", map[string]any{
					"index": index,
					"delta": map[string]any{"type": "input_json_delta", "partial_json": chunk},
				}) {
					return
				}
			}
		}

		if !writeEvent("content_block_stop", map[string]any{"index": index}) {
			return
		}
	}

	stopReason := scripted.StopReason
	if stopReason == "" {
		if hasTool {
			stopReason = "tool_use"
		} else {
			stopReason = "end_turn"
		}
	}
	var stopDetails any
	if stopReason == "refusal" {
		stopDetails = map[string]any{
			"type":        "refusal",
			"category":    scripted.RefusalCategory,
			"explanation": "scripted refusal",
		}
	}
	if !writeEvent("message_delta", map[string]any{
		"delta": map[string]any{
			"stop_reason":   stopReason,
			"stop_sequence": nil,
			"stop_details":  stopDetails,
		},
		"usage": map[string]any{"output_tokens": scripted.OutputTokens},
	}) {
		return
	}

	writeEvent("message_stop", map[string]any{})
}

func (f *FakeMessages) handleModels(w http.ResponseWriter, r *http.Request) {
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
		_, _ = io.WriteString(w, `{"type":"error","error":{"type":"authentication_error","message":"scripted failure"}}`)
		return
	}

	data := make([]map[string]string, 0, len(ids))
	for _, id := range ids {
		data = append(data, map[string]string{"id": id, "type": "model", "display_name": id})
	}
	if err := json.NewEncoder(w).Encode(map[string]any{"data": data, "has_more": false}); err != nil {
		f.t.Errorf("encode models response: %v", err)
	}
}

func blockGroupCount(block FakeBlock) int {
	groups := 0
	if block.Thinking != "" || block.Signature != "" {
		groups++
	}
	if block.Text != "" {
		groups++
	}
	if block.ToolID != "" || block.ToolName != "" || block.ToolInputJSON != "" || len(block.ToolInputChunks) > 0 {
		groups++
	}
	return groups
}
