package chatcompletions

import (
	"context"
	"errors"
	"net"
	"sync/atomic"
	"testing"
	"time"

	openai "github.com/sashabaranov/go-openai"
)

// TestIsRetryableCoversTransportFailures pins the retry policy against the one
// both SDKs apply for this adapter's siblings: a request that never reached a
// server is worth another attempt, a caller's cancellation is not, and a status
// the endpoint will keep returning is not.
func TestIsRetryableCoversTransportFailures(t *testing.T) {
	tests := []struct {
		name string
		err  error
		want bool
	}{
		{
			// The SDK builds a RequestError from a response it received, so this
			// shape stands for a response whose status it could not use. A
			// connection that never reached a server is a *url.Error, which
			// TestChatTurnRetriesAConnectionThatNeverReachedAServer drives for
			// real rather than constructing here.
			name: "request error with no usable status",
			err:  &openai.RequestError{Err: errors.New("connection reset by peer")},
			want: true,
		},
		{
			name: "429 is retryable",
			err:  &openai.RequestError{HTTPStatusCode: 429, Err: errors.New("slow down")},
			want: true,
		},
		{
			name: "503 is retryable",
			err:  &openai.APIError{HTTPStatusCode: 503},
			want: true,
		},
		{
			name: "400 is not retryable",
			err:  &openai.APIError{HTTPStatusCode: 400},
			want: false,
		},
		{
			name: "cancellation is not retryable",
			err:  &openai.RequestError{Err: context.Canceled},
			want: false,
		},
		{
			name: "expired deadline is not retryable",
			err:  &openai.RequestError{Err: context.DeadlineExceeded},
			want: false,
		},
		{
			name: "an unrelated error is not retryable",
			err:  errors.New("boom"),
			want: false,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := isRetryable(test.err); got != test.want {
				t.Errorf("isRetryable(%v) = %v, want %v", test.err, got, test.want)
			}
		})
	}
}

// TestDefaultBackoffJittersAroundTheBase pins the schedule: each attempt waits
// between half and all of 1s, 2s, 4s. The jitter is what keeps concurrent
// sessions that failed against one endpoint from retrying in lockstep.
func TestDefaultBackoffJittersAroundTheBase(t *testing.T) {
	for attempt, base := range []time.Duration{time.Second, 2 * time.Second, 4 * time.Second} {
		seen := map[time.Duration]bool{}
		for i := 0; i < 200; i++ {
			got := defaultBackoff(attempt)
			if got < base/2 || got > base {
				t.Fatalf("defaultBackoff(%d) = %v, want between %v and %v", attempt, got, base/2, base)
			}
			seen[got] = true
		}
		if len(seen) < 2 {
			t.Errorf("defaultBackoff(%d) returned one value over 200 draws, want a jittered schedule", attempt)
		}
	}
}

// TestChatTurnRetriesAConnectionThatNeverReachedAServer pins the retry against a
// real transport failure rather than a hand-built error: the listener accepts the
// connection and closes it without a response, which is the shape a refused or
// reset stream setup takes. Every attempt must reach the listener.
func TestChatTurnRetriesAConnectionThatNeverReachedAServer(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer listener.Close()

	var attempts atomic.Int64
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			attempts.Add(1)
			_ = conn.Close()
		}
	}()

	client := NewClient("key", "http://"+listener.Addr().String())
	client.Backoff = func(int) time.Duration { return 0 }

	_, err = client.ChatTurn(context.Background(), openai.ChatCompletionRequest{
		Model:    "m",
		Messages: []openai.ChatCompletionMessage{{Role: openai.ChatMessageRoleUser, Content: "hi"}},
	}, func(string) {})
	if err == nil {
		t.Fatal("ChatTurn() succeeded against a listener that never answers")
	}
	if !isRetryable(err) {
		t.Errorf("isRetryable(%v) = false, want true: the connection never reached a server", err)
	}

	// maxAttempts is 4; a loop that does not retry a transport failure stops at 1.
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) && attempts.Load() < 4 {
		time.Sleep(10 * time.Millisecond)
	}
	if got := attempts.Load(); got != 4 {
		t.Errorf("connection attempts = %d, want 4 (stream setup must be retried when the connection fails)", got)
	}
}
