package messages

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/provider/chatcompletions"
	"github.com/Geek0x0/subagent-mcp/internal/provider/responses"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

// TestUsageMappingIsOneMeaningAcrossAdapters pins the single meaning of
// provider.Usage for every adapter, cache in play: Input is the full prompt
// INCLUDING cached tokens, Cached is the subset of Input served from cache,
// Reasoning is the API's reasoning/thinking count, and Total is Input+Output.
func TestUsageMappingIsOneMeaningAcrossAdapters(t *testing.T) {
	const (
		input     = 100 // full prompt, cache creation included
		cached    = 50  // served from cache; a subset of input
		cacheRead = 50
		cacheNew  = 25 // messages reports cache creation separately
		fresh     = 25 // input - cacheRead - cacheNew
		output    = 20
		reasoning = 7
	)
	want := provider.Usage{
		Input: input, Cached: cached, Output: output, Reasoning: reasoning, Total: input + output,
	}

	// chat-completions reports its totals on a usage-only final chunk.
	chatServer := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		for _, data := range []string{
			`{"choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}`,
			fmt.Sprintf(`{"choices":[],"usage":{"prompt_tokens":%d,"completion_tokens":%d,"total_tokens":%d,`+
				`"prompt_tokens_details":{"cached_tokens":%d},"completion_tokens_details":{"reasoning_tokens":%d}}}`,
				input, output, input+output, cached, reasoning),
		} {
			fmt.Fprintf(w, "data: %s\n\n", data)
		}
		fmt.Fprint(w, "data: [DONE]\n\n")
	}))
	t.Cleanup(chatServer.Close)

	newResponses := func(t *testing.T) provider.Provider {
		t.Helper()
		fake := testutil.NewFakeResponses(t, []testutil.FakeResponse{{
			Items:           []testutil.FakeResponseItem{{MessageText: "hi"}},
			InputTokens:     input,
			CachedTokens:    cached,
			OutputTokens:    output,
			ReasoningTokens: reasoning,
		}})
		p, err := responses.New("openai", config.Provider{API: config.APIResponses, BaseURL: fake.URL}, "k")
		if err != nil {
			t.Fatal(err)
		}
		return p
	}

	for _, tc := range []struct {
		name string
		turn func(t *testing.T) (*provider.TurnResult, error)
	}{
		{"chat-completions", func(t *testing.T) (*provider.TurnResult, error) {
			p, err := chatcompletions.New("openai", config.Provider{
				API: config.APIChatCompletions, BaseURL: chatServer.URL,
			}, "k")
			if err != nil {
				return nil, err
			}
			return p.Turn(context.Background(), provider.TurnRequest{
				Model: "deepseek-chat", Messages: userOnly(),
			}, nil)
		}},
		{"responses", func(t *testing.T) (*provider.TurnResult, error) {
			return newResponses(t).Turn(context.Background(), provider.TurnRequest{
				Model: "gpt-x", Messages: userOnly(),
			}, nil)
		}},
		{"messages", func(t *testing.T) (*provider.TurnResult, error) {
			fake := testutil.NewFakeMessages(t, []testutil.FakeMessage{{
				Blocks:              []testutil.FakeBlock{{Text: "hi"}},
				InputTokens:         fresh,
				CacheReadTokens:     cacheRead,
				CacheCreationTokens: cacheNew,
				OutputTokens:        output,
				ThinkingTokens:      reasoning,
			}})
			return newAdapter(t, fake, 1000).Turn(context.Background(), provider.TurnRequest{
				Model: "claude-x", Messages: userOnly(),
			}, nil)
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			res, err := tc.turn(t)
			if err != nil {
				t.Fatal(err)
			}
			got := *res.Usage
			if got != want {
				t.Fatalf("usage = %#v, want %#v", got, want)
			}
			if got.Cached > got.Input {
				t.Fatalf("Cached %d must be a subset of Input %d", got.Cached, got.Input)
			}
			if got.Total != got.Input+got.Output {
				t.Fatalf("Total %d != Input %d + Output %d", got.Total, got.Input, got.Output)
			}
			if got.Reasoning != reasoning {
				t.Fatalf("Reasoning = %d, want %d", got.Reasoning, reasoning)
			}
		})
	}
}
