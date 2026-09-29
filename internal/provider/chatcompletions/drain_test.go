package chatcompletions

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	openai "github.com/sashabaranov/go-openai"
)

// A server that sends the final sentinel but keeps the response open must not
// stall the turn: draining the body on Close is bounded by time.
func TestChatTurnReturnsWhenServerKeepsStreamOpenAfterDone(t *testing.T) {
	block := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.Write([]byte("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"))
		w.(http.Flusher).Flush()
		<-block
	}))
	defer srv.Close()
	defer close(block)
	c := NewClient("k", srv.URL)
	done := make(chan error, 1)
	go func() {
		_, err := c.ChatTurn(context.Background(), openai.ChatCompletionRequest{Model: "m"}, nil)
		done <- err
	}()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("ChatTurn() error = %v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("ChatTurn hung after [DONE]")
	}
}
