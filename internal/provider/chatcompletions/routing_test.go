package chatcompletions_test

import (
	"context"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/provider/chatcompletions"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

// routedStream is the shape an OpenRouter-style gateway streams: the serving
// upstream rides in the "provider" field of every chunk.
const routedStream = `data: {"id":"g1","provider":"StreamLake","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}` + "\n\n" +
	`data: {"id":"g1","provider":"StreamLake","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}` + "\n\n" +
	"data: [DONE]\n\n"

func TestExtraBodyReachesTheRequest(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{SSE: routedStream}})
	extra := map[string]any{
		"provider": map[string]any{"ignore": []any{"relace"}, "sort": "throughput"},
		"seed":     int64(7),
	}
	client := chatcompletions.NewClientWithBody("k", fake.URL, extra)
	client.Backoff = func(int) time.Duration { return 0 }

	if _, err := client.ChatTurn(context.Background(), chatRequest(), nil); err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}
	body := fake.Request(0)
	provider, ok := body["provider"].(map[string]any)
	if !ok || provider["sort"] != "throughput" {
		t.Fatalf("request body provider = %#v, want the extra routing object", body["provider"])
	}
	if ignore, _ := provider["ignore"].([]any); len(ignore) != 1 || ignore[0] != "relace" {
		t.Errorf("provider.ignore = %#v, want [relace]", provider["ignore"])
	}
	if body["seed"] != float64(7) {
		t.Errorf("seed = %#v, want 7", body["seed"])
	}
	// The SDK's own fields are untouched.
	if body["model"] != "deepseek-chat" || body["stream"] != true {
		t.Errorf("model/stream = %#v/%#v, want the SDK values", body["model"], body["stream"])
	}
	if _, ok := body["messages"].([]any); !ok {
		t.Errorf("messages missing from %#v", body)
	}
}

func TestNoExtraBodyLeavesTheRequestAlone(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{SSE: routedStream}})
	client := newTestClient(fake.URL)
	if _, err := client.ChatTurn(context.Background(), chatRequest(), nil); err != nil {
		t.Fatalf("ChatTurn() error = %v", err)
	}
	if _, ok := fake.Request(0)["provider"]; ok {
		t.Errorf("request carries a provider field without extra_body: %#v", fake.Request(0))
	}
}

// The upstream the gateway names is reported with the turn.
func TestTurnReportsTheServingUpstream(t *testing.T) {
	fake := testutil.NewFakeChat(t, []testutil.FakeTurn{{SSE: routedStream}, {Text: "plain server"}})
	client := newTestClient(fake.URL)

	result, err := client.ChatTurn(context.Background(), chatRequest(), nil)
	if err != nil || result.Upstream != "StreamLake" {
		t.Fatalf("ChatTurn() = (%+v, %v), want Upstream StreamLake", result, err)
	}
	result, err = client.ChatTurn(context.Background(), chatRequest(), nil)
	if err != nil || result.Upstream != "" {
		t.Fatalf("ChatTurn() against a server that names none = (%+v, %v), want an empty Upstream", result, err)
	}
}
