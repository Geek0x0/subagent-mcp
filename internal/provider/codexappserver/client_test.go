package codexappserver_test

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider/codexappserver"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func TestClientCallRoundTrip(t *testing.T) {
	f := testutil.NewFakeCodex(t)
	f.Handle("model/list", func(params json.RawMessage) (any, error) {
		return map[string]any{"data": []any{map[string]string{"id": "m"}}}, nil
	})
	r, w := f.ClientSide()
	var wire bytes.Buffer
	c := startClient(t, f, r, io.MultiWriter(w, &wire))
	var result struct {
		Data []struct{ ID string }
	}
	if err := c.Call(testContext(t), "model/list", map[string]string{"cursor": ""}, &result); err != nil {
		t.Fatal(err)
	}
	if len(result.Data) != 1 || result.Data[0].ID != "m" {
		t.Fatalf("result = %+v, want model m", result)
	}
	if wire.String()[wire.Len()-1] != '\n' || bytes.Count(wire.Bytes(), []byte{'\n'}) != 1 {
		t.Fatalf("request is not one JSON line: %q", wire.String())
	}
	var request map[string]json.RawMessage
	if err := json.Unmarshal(wire.Bytes(), &request); err != nil {
		t.Fatal(err)
	}
	if _, ok := request["jsonrpc"]; ok {
		t.Fatalf("request contains jsonrpc: %s", wire.Bytes())
	}
	if string(request["id"]) != "1" || string(request["method"]) != `"model/list"` {
		t.Fatalf("request = %s", wire.Bytes())
	}
	if got := f.Received("model/list"); len(got) != 1 || string(got[0]) != `{"cursor":""}` {
		t.Fatalf("Received(model/list) = %s", got)
	}
}

func TestClientCallError(t *testing.T) {
	c, f := newTestClient(t)
	f.Handle("model/list", func(json.RawMessage) (any, error) {
		return nil, errors.New("scripted failure")
	})
	err := c.Call(testContext(t), "model/list", nil, nil)
	var rpcErr *codexappserver.RPCError
	if !errors.As(err, &rpcErr) || rpcErr.Message != "scripted failure" {
		t.Fatalf("Call error = %v, want *RPCError with scripted failure", err)
	}
	err = c.Call(testContext(t), "unknown", nil, nil)
	if !errors.As(err, &rpcErr) || rpcErr.Code != -32601 {
		t.Fatalf("unhandled Call error = %v, want -32601", err)
	}
}

func TestClientRoutesByThread(t *testing.T) {
	c, f := newTestClient(t)
	a, b := c.Subscribe("a"), c.Subscribe("b")
	if c.Subscribe("a") != a {
		t.Fatal("Subscribe is not idempotent")
	}
	f.Notify("account/updated", map[string]string{"status": "ok"})
	f.Notify("unknown/event", map[string]string{"threadId": "nobody"})
	f.Notify("item/delta", map[string]any{"threadId": "a", "seq": 1})
	f.Notify("item/delta", map[string]any{"threadId": "b", "seq": 2})
	f.Notify("item/delta", map[string]any{"threadId": "a", "seq": 3})
	ctx := testContext(t)
	for _, want := range []struct {
		sub    *codexappserver.Subscription
		thread string
		seq    int
	}{{a, "a", 1}, {a, "a", 3}, {b, "b", 2}} {
		msg, err := want.sub.Next(ctx)
		if err != nil {
			t.Fatal(err)
		}
		var params struct {
			ThreadID string `json:"threadId"`
			Seq      int    `json:"seq"`
		}
		if err := json.Unmarshal(msg.Params, &params); err != nil {
			t.Fatal(err)
		}
		if msg.Method != "item/delta" || len(msg.ID) != 0 || params.ThreadID != want.thread || params.Seq != want.seq {
			t.Fatalf("message = %+v (%s), want thread %s seq %d", msg, msg.Params, want.thread, want.seq)
		}
	}
}

func TestClientReaderNeverBlocks(t *testing.T) {
	c, f := newTestClient(t)
	c.Subscribe("slow") // Deliberately never consumed, even during cleanup.
	fast := c.Subscribe("fast")
	sent := make(chan struct{})
	go func() {
		defer close(sent)
		for i := range 10_000 {
			f.Notify("item/delta", map[string]any{"threadId": "slow", "seq": i})
		}
		f.Notify("item/delta", map[string]string{"threadId": "fast"})
	}()
	msg, err := fast.Next(testContext(t))
	if err != nil {
		t.Fatalf("fast.Next did not return within 2s: %v", err)
	}
	if string(msg.Params) != `{"threadId":"fast"}` {
		t.Fatalf("fast received %s", msg.Params)
	}
	receive(t, sent)
}

func TestClientAnswersUnroutedServerRequests(t *testing.T) {
	c, f := newTestClient(t)
	c.Subscribe("active")
	for _, tc := range []struct {
		method string
		params any
	}{
		{"attestation/generate", map[string]any{}},
		{"item/tool/requestUserInput", map[string]string{"threadId": "nobody"}},
		{"unknown/request", map[string]any{"threadId": 123}},
	} {
		t.Run(tc.method, func(t *testing.T) {
			reply := make(chan json.RawMessage, 1)
			go func() { reply <- f.Request(tc.method, tc.params) }()
			assertUnsupported(t, receive(t, reply))
		})
	}
}

func TestClientServerRequestDelivered(t *testing.T) {
	c, f := newTestClient(t)
	sub := c.Subscribe("t")
	reply := make(chan json.RawMessage, 1)
	go func() {
		reply <- f.Request("item/commandExecution/requestApproval", map[string]string{
			"threadId": "t", "turnId": "turn-1", "command": "pwd",
		})
	}()
	msg, err := sub.Next(testContext(t))
	if err != nil {
		t.Fatal(err)
	}
	if len(msg.ID) == 0 || msg.Method != "item/commandExecution/requestApproval" {
		t.Fatalf("server request = %+v", msg)
	}
	if err := c.Respond(msg.ID, map[string]string{"decision": "accept"}); err != nil {
		t.Fatal(err)
	}
	var response struct {
		ID     json.RawMessage
		Result struct{ Decision string }
		Error  *codexappserver.RPCError
	}
	if err := json.Unmarshal(receive(t, reply), &response); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(response.ID, msg.ID) || response.Error != nil || response.Result.Decision != "accept" {
		t.Fatalf("server received %+v, want accept for id %s", response, msg.ID)
	}
}

func TestClientEOF(t *testing.T) {
	baseline := runtime.NumGoroutine()
	c, f := newTestClient(t)
	callErr, nextErr := pendingCallAndNext(t, c, f)
	f.Crash()
	if err := receive(t, callErr); !errors.Is(err, io.EOF) {
		t.Fatalf("Call error = %v, want wrapped EOF", err)
	}
	if err := receive(t, nextErr); !errors.Is(err, io.EOF) {
		t.Fatalf("Next error = %v, want EOF", err)
	}
	receive(t, c.Done())
	if !errors.Is(c.Err(), io.EOF) {
		t.Fatalf("Err = %v, want EOF", c.Err())
	}
	if err := c.Call(testContext(t), "after/exit", nil, nil); !errors.Is(err, io.EOF) {
		t.Fatalf("Call after EOF = %v", err)
	}
	if _, err := c.Subscribe("after-exit").Next(testContext(t)); !errors.Is(err, io.EOF) {
		t.Fatalf("Next after EOF = %v", err)
	}
	if _, err := c.Subscribe("blocked").Next(testContext(t)); !errors.Is(err, io.EOF) {
		t.Fatalf("queued Next after EOF = %v", err)
	}
	deadline := time.Now().Add(2 * time.Second)
	for runtime.NumGoroutine() > baseline && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if got := runtime.NumGoroutine(); got > baseline {
		t.Fatalf("goroutines after EOF = %d, baseline = %d", got, baseline)
	}
}

func TestClientConcurrentCalls(t *testing.T) {
	c, f := newTestClient(t)
	const count = 16
	arrived := make(chan struct{}, count)
	gates := make([]chan struct{}, count)
	for i := range gates {
		gates[i] = make(chan struct{})
		t.Cleanup(func() { close(gates[i]) })
	}
	f.Handle("echo", func(params json.RawMessage) (any, error) {
		var seq int
		if err := json.Unmarshal(params, &seq); err != nil {
			return nil, err
		}
		arrived <- struct{}{}
		<-gates[seq]
		return seq, nil
	})
	results := make([]chan error, count)
	ctx := testContext(t)
	for i := range count {
		results[i] = make(chan error, 1)
		go func() {
			var got int
			err := c.Call(ctx, "echo", i, &got)
			if err == nil && got != i {
				err = fmt.Errorf("Call(%d) got %d", i, got)
			}
			results[i] <- err
		}()
	}
	for range count {
		receive(t, arrived)
	}
	// Complete calls out of order, independently of their request IDs.
	for i := count - 1; i >= 0; i-- {
		gates[i] <- struct{}{}
		if err := receive(t, results[i]); err != nil {
			t.Fatal(err)
		}
	}
}

func TestClientCancellation(t *testing.T) {
	c, f := newTestClient(t)
	entered, release := make(chan struct{}), make(chan struct{})
	t.Cleanup(func() { close(release) })
	f.Handle("wait", func(json.RawMessage) (any, error) {
		close(entered)
		<-release
		return nil, nil
	})
	ctx, cancel := context.WithCancel(testContext(t))
	defer cancel()
	callErr := make(chan error, 1)
	go func() { callErr <- c.Call(ctx, "wait", nil, nil) }()
	receive(t, entered)
	cancel()
	if err := receive(t, callErr); !errors.Is(err, context.Canceled) {
		t.Fatalf("Call = %v, want canceled", err)
	}
	if _, err := c.Subscribe("t").Next(ctx); !errors.Is(err, context.Canceled) {
		t.Fatalf("Next = %v, want canceled", err)
	}
	if err := c.Call(ctx, "not-sent", nil, nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("already canceled Call = %v", err)
	}
	if got := f.Received("not-sent"); len(got) != 0 {
		t.Fatalf("canceled request was sent: %s", got)
	}
	if err := c.Call(testContext(t), "initialize", nil, nil); err != nil {
		t.Fatalf("subsequent Call: %v", err)
	}
}

func TestClientUnsubscribe(t *testing.T) {
	c, f := newTestClient(t)
	sub := c.Subscribe("t")
	nextErr := make(chan error, 1)
	ctx := testContext(t)
	go func() {
		_, err := sub.Next(ctx)
		nextErr <- err
	}()
	c.Unsubscribe("t")
	c.Unsubscribe("t")
	if err := receive(t, nextErr); err == nil {
		t.Fatal("unsubscribed Next succeeded")
	}
	reply := make(chan json.RawMessage, 1)
	go func() { reply <- f.Request("approval", map[string]string{"threadId": "t"}) }()
	assertUnsupported(t, receive(t, reply))
	if c.Subscribe("t") == sub {
		t.Fatal("Subscribe reused an unsubscribed queue")
	}
}

func TestClientLargeMessages(t *testing.T) {
	c, f := newTestClient(t)
	large := strings.Repeat("x", 256*1024)
	f.Handle("large", func(json.RawMessage) (any, error) { return large, nil })
	var result string
	if err := c.Call(testContext(t), "large", nil, &result); err != nil || result != large {
		t.Fatalf("large Call: len = %d, error = %v", len(result), err)
	}
	sub := c.Subscribe("t")
	f.Notify("large", map[string]string{"threadId": "t", "text": large})
	msg, err := sub.Next(testContext(t))
	if err != nil {
		t.Fatal(err)
	}
	var params struct{ Text string }
	if err := json.Unmarshal(msg.Params, &params); err != nil || params.Text != large {
		t.Fatalf("large notification: len = %d, error = %v", len(params.Text), err)
	}
}

func TestClientDecodeError(t *testing.T) {
	f := testutil.NewFakeCodex(t)
	r, w := f.ClientSide()
	c := startClient(t, f, io.MultiReader(r, strings.NewReader("{invalid\n")), w)
	callErr, nextErr := pendingCallAndNext(t, c, f)
	f.Crash()
	for _, err := range []error{receive(t, callErr), receive(t, nextErr)} {
		var syntaxErr *json.SyntaxError
		if !errors.As(err, &syntaxErr) {
			t.Fatalf("error = %v, want JSON syntax error", err)
		}
	}
	receive(t, c.Done())
	var syntaxErr *json.SyntaxError
	if !errors.As(c.Err(), &syntaxErr) {
		t.Fatalf("Err = %v, want JSON syntax error", c.Err())
	}
}

func TestClientInitializeAndNotify(t *testing.T) {
	c, f := newTestClient(t)
	var result map[string]any
	if err := c.Call(testContext(t), "initialize", map[string]any{
		"clientInfo": map[string]string{"name": "test", "version": "1"},
	}, &result); err != nil {
		t.Fatal(err)
	}
	if err := c.Notify("initialized", nil); err != nil {
		t.Fatal(err)
	}
	// A later response is a barrier for the fake's request recording.
	if err := c.Call(testContext(t), "initialize", nil, nil); err != nil {
		t.Fatal(err)
	}
	if got := f.Received("initialized"); len(got) != 1 {
		t.Fatalf("Received(initialized) = %s", got)
	}
}

func pendingCallAndNext(t *testing.T, c *codexappserver.Client, f *testutil.FakeCodex) (<-chan error, <-chan error) {
	t.Helper()
	c.Subscribe("blocked")
	pending := make(chan struct{})
	f.Handle("pending", func(json.RawMessage) (any, error) {
		close(pending)
		// Crash must unblock the fake's Request as well as the client's Call.
		f.Request("approval", map[string]string{"threadId": "blocked"})
		return nil, nil
	})
	ctx := testContext(t)
	callErr, nextErr := make(chan error, 1), make(chan error, 1)
	go func() { callErr <- c.Call(ctx, "pending", nil, nil) }()
	sub := c.Subscribe("waiting")
	go func() {
		_, err := sub.Next(ctx)
		nextErr <- err
	}()
	receive(t, pending)
	return callErr, nextErr
}

func assertUnsupported(t *testing.T, raw json.RawMessage) {
	t.Helper()
	var reply struct {
		ID      json.RawMessage
		Error   *codexappserver.RPCError
		JSONRPC json.RawMessage
	}
	if err := json.Unmarshal(raw, &reply); err != nil {
		t.Fatal(err)
	}
	if len(reply.ID) == 0 || reply.Error == nil || reply.Error.Code != -32601 || reply.Error.Message != "not supported by subagent-mcp" {
		t.Fatalf("unsupported reply = %s", raw)
	}
	if len(reply.JSONRPC) != 0 {
		t.Fatalf("reply contains jsonrpc: %s", raw)
	}
}

func newTestClient(t *testing.T) (*codexappserver.Client, *testutil.FakeCodex) {
	t.Helper()
	f := testutil.NewFakeCodex(t)
	r, w := f.ClientSide()
	return startClient(t, f, r, w), f
}

func startClient(t *testing.T, f *testutil.FakeCodex, r io.Reader, w io.Writer) *codexappserver.Client {
	t.Helper()
	c := codexappserver.NewClient(r, w)
	t.Cleanup(func() {
		f.Crash()
		receive(t, c.Done())
	})
	return c
}

func testContext(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(t.Context(), 2*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func receive[T any](t *testing.T, ch <-chan T) T {
	t.Helper()
	select {
	case value := <-ch:
		return value
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for result")
		var zero T
		return zero
	}
}
