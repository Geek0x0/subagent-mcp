package codexappserver

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"testing"
)

// TestSubscriptionMergesStreamedDeltas pins the queue's bound on a streamed
// turn: one notification per token from the app-server becomes one entry per run
// of deltas for an item, and the consumer still sees the whole text because the
// run is folded back into the notification it dequeues.
func TestSubscriptionMergesStreamedDeltas(t *testing.T) {
	sub := &Subscription{changed: make(chan struct{})}
	for _, delta := range []string{"Hel", "lo ", "wor", "ld"} {
		sub.enqueue(deltaNotification("item-1", delta))
	}

	sub.mu.Lock()
	queued := len(sub.queue)
	sub.mu.Unlock()
	if queued != 1 {
		t.Fatalf("queued notifications = %d, want 1 merged entry for the delta run", queued)
	}

	msg, err := sub.Next(context.Background())
	if err != nil {
		t.Fatalf("Next() error = %v", err)
	}
	var params notificationParams
	if err := json.Unmarshal(msg.Params, &params); err != nil {
		t.Fatalf("unmarshal merged params: %v", err)
	}
	if params.Delta != "Hello world" {
		t.Errorf("merged delta = %q, want %q", params.Delta, "Hello world")
	}
	if params.ItemID != "item-1" {
		t.Errorf("merged itemId = %q, want item-1", params.ItemID)
	}
}

// TestSubscriptionKeepsDeltaRunsApart pins that merging is per item and only
// adjacent: text from another item, or a notification in between, must not be
// folded into the wrong message.
func TestSubscriptionKeepsDeltaRunsApart(t *testing.T) {
	sub := &Subscription{changed: make(chan struct{})}
	sub.enqueue(deltaNotification("item-1", "a"))
	sub.enqueue(deltaNotification("item-1", "b"))
	sub.enqueue(deltaNotification("item-2", "c"))
	sub.enqueue(Message{Method: "turn/completed"})
	sub.enqueue(deltaNotification("item-1", "d"))

	sub.mu.Lock()
	queued := len(sub.queue)
	sub.mu.Unlock()
	if queued != 4 {
		t.Fatalf("queued notifications = %d, want 4 (two runs, an item change, and a non-delta)", queued)
	}

	wantDeltas := []string{"ab", "c", "", "d"}
	for i, want := range wantDeltas {
		msg, err := sub.Next(context.Background())
		if err != nil {
			t.Fatalf("Next() %d error = %v", i, err)
		}
		if want == "" {
			if msg.Method != "turn/completed" {
				t.Fatalf("message %d = %q, want turn/completed between the runs", i, msg.Method)
			}
			continue
		}
		var params notificationParams
		if err := json.Unmarshal(msg.Params, &params); err != nil {
			t.Fatalf("unmarshal %d: %v", i, err)
		}
		if params.Delta != want {
			t.Errorf("message %d delta = %q, want %q", i, params.Delta, want)
		}
	}
}

// TestSubscriptionFailsTheTurnWhenTheBacklogGrows pins the backstop: a consumer
// that never drains must fail the turn with an actionable error rather than let
// the queue grow until the process dies.
func TestSubscriptionFailsTheTurnWhenTheBacklogGrows(t *testing.T) {
	sub := &Subscription{changed: make(chan struct{})}
	for i := 0; i <= maxQueuedEvents; i++ {
		// A distinct item each time, so nothing merges and the queue really grows.
		sub.enqueue(deltaNotification(fmt.Sprintf("item-%d", i), "d"))
	}

	if _, err := sub.Next(context.Background()); err == nil || !strings.Contains(err.Error(), "backlog exceeded") {
		t.Fatalf("Next() error = %v, want the backlog error", err)
	}
}

// deltaNotification builds the item/agentMessage/delta notification the
// app-server sends once per streamed token.
func deltaNotification(itemID, delta string) Message {
	params, _ := json.Marshal(map[string]string{
		"threadId": "thread-1", "itemId": itemID, "delta": delta,
	})
	return Message{Method: deltaMethod, Params: params}
}

// TestSubscriptionBacklogIsRecoverable pins that hitting the cap closes that
// queue for good but does not poison the thread for the life of the process: a
// later Subscribe must hand back a working queue rather than the failed one.
func TestSubscriptionBacklogIsRecoverable(t *testing.T) {
	// A reader that never produces input, so the client stays running instead of
	// failing its subscriptions with EOF.
	reader, writer := io.Pipe()
	defer writer.Close()
	client := NewClient(reader, io.Discard)

	sub := client.Subscribe("thread-cap")

	for i := 0; i <= maxQueuedEvents; i++ {
		sub.enqueue(deltaNotification(fmt.Sprintf("item-%d", i), "d"))
	}
	if _, err := sub.Next(context.Background()); err == nil || !strings.Contains(err.Error(), "backlog exceeded") {
		t.Fatalf("Next() error = %v, want the backlog error", err)
	}
	if !strings.Contains(sub.err.Error(), "no longer delivered") {
		t.Errorf("backlog error = %q, want it to say the events stop being delivered", sub.err)
	}

	replacement := client.Subscribe("thread-cap")
	if replacement == sub {
		t.Fatal("Subscribe() returned the failed queue, so the thread could never receive events again")
	}
	replacement.enqueue(deltaNotification("item-1", "ok"))
	msg, err := replacement.Next(context.Background())
	if err != nil {
		t.Fatalf("Next() on the replacement queue error = %v", err)
	}
	var params notificationParams
	if err := json.Unmarshal(msg.Params, &params); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if params.Delta != "ok" {
		t.Errorf("replacement delta = %q, want ok", params.Delta)
	}
}
