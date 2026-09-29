package chatcompletions

import (
	"context"
	"errors"
	"io"
	"net/http"
	"sort"
	"strings"
	"time"

	openai "github.com/sashabaranov/go-openai"

	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

type TurnResult struct {
	Content   string
	Reasoning string
	ToolCalls []openai.ToolCall
	Usage     *openai.Usage
}

type Client struct {
	oai     *openai.Client
	Backoff func(attempt int) time.Duration
}

// NewClient builds a low-level Chat Completions client.
func NewClient(apiKey, baseURL string) *Client {
	cfg := openai.DefaultConfig(apiKey)
	cfg.BaseURL = baseURL
	// go-openai's DefaultConfig gives every client an &http.Client{} whose
	// nil Transport resolves to the process-global http.DefaultTransport at
	// request time. Bind the client to the pool registered for this base URL
	// instead: sessions against one endpoint reuse its connections without
	// coupling the endpoint (or other endpoints) to the process-wide default.
	// The client itself stays per adapter, carrying this session's key.
	cfg.HTTPClient = &http.Client{
		Transport: provider.SharedTransport(provider.KindChatCompletions, baseURL, 0),
	}

	return &Client{
		oai:     openai.NewClientWithConfig(cfg),
		Backoff: defaultBackoff,
	}
}

func defaultBackoff(attempt int) time.Duration {
	return time.Duration(1<<uint(attempt)) * time.Second
}

func (c *Client) ChatTurn(
	ctx context.Context,
	req openai.ChatCompletionRequest,
	onDelta func(string),
) (*TurnResult, error) {
	streamRequest := req
	streamRequest.Stream = true
	streamRequest.StreamOptions = &openai.StreamOptions{IncludeUsage: true}

	// ponytail: Retries cover stream setup only. A mid-stream receive error fails
	// the turn; callers can issue a fresh turn, while resumption would require
	// protocol-level replay support.
	stream, err := c.openStreamWithRetry(ctx, streamRequest)
	if err != nil {
		return nil, err
	}
	defer stream.Close()

	var content strings.Builder
	var reasoning strings.Builder
	toolCallsByIndex := make(map[int]*openai.ToolCall)
	var usage *openai.Usage

	for {
		response, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, err
		}

		if response.Usage != nil {
			latestUsage := *response.Usage
			usage = &latestUsage
		}

		for _, choice := range response.Choices {
			delta := choice.Delta
			if delta.ReasoningContent != "" {
				reasoning.WriteString(delta.ReasoningContent)
			}
			if delta.Content != "" {
				content.WriteString(delta.Content)
				if onDelta != nil {
					onDelta(delta.Content)
				}
			}

			for chunkPosition, entry := range delta.ToolCalls {
				index := chunkPosition
				if entry.Index != nil {
					index = *entry.Index
				} else {
					// ponytail: Chunk position recovers the common single-call case
					// when a provider omits Index. Selectively omitted indices in a
					// multi-call batch can still misassign continuations; robust
					// recovery would require provider-supplied correlation metadata.
				}

				call, ok := toolCallsByIndex[index]
				if !ok {
					toolType := entry.Type
					if toolType == "" {
						toolType = openai.ToolTypeFunction
					}

					toolCallsByIndex[index] = &openai.ToolCall{
						ID:   entry.ID,
						Type: toolType,
						Function: openai.FunctionCall{
							Name:      entry.Function.Name,
							Arguments: entry.Function.Arguments,
						},
					}
					continue
				}

				call.Function.Arguments += entry.Function.Arguments
			}
		}
	}

	return &TurnResult{
		Content:   content.String(),
		Reasoning: reasoning.String(),
		ToolCalls: sortedToolCalls(toolCallsByIndex),
		Usage:     usage,
	}, nil
}

func (c *Client) openStreamWithRetry(
	ctx context.Context,
	req openai.ChatCompletionRequest,
) (*openai.ChatCompletionStream, error) {
	const maxAttempts = 4

	for attempt := 0; attempt < maxAttempts; attempt++ {
		stream, err := c.oai.CreateChatCompletionStream(ctx, req)
		if err == nil {
			return stream, nil
		}
		if attempt == maxAttempts-1 || !isRetryable(err) {
			return nil, err
		}

		if err := waitForBackoff(ctx, c.Backoff(attempt)); err != nil {
			return nil, err
		}
	}

	panic("unreachable")
}

func isRetryable(err error) bool {
	var apiErr *openai.APIError
	if errors.As(err, &apiErr) && isRetryableStatus(apiErr.HTTPStatusCode) {
		return true
	}

	var requestErr *openai.RequestError
	return errors.As(err, &requestErr) && isRetryableStatus(requestErr.HTTPStatusCode)
}

func isRetryableStatus(status int) bool {
	return status == http.StatusTooManyRequests || status >= http.StatusInternalServerError
}

func waitForBackoff(ctx context.Context, delay time.Duration) error {
	if err := ctx.Err(); err != nil {
		return err
	}

	timer := time.NewTimer(delay)
	defer timer.Stop()

	select {
	case <-timer.C:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func sortedToolCalls(callsByIndex map[int]*openai.ToolCall) []openai.ToolCall {
	if len(callsByIndex) == 0 {
		return nil
	}

	indices := make([]int, 0, len(callsByIndex))
	for index := range callsByIndex {
		indices = append(indices, index)
	}
	sort.Ints(indices)

	calls := make([]openai.ToolCall, 0, len(indices))
	for _, index := range indices {
		calls = append(calls, *callsByIndex[index])
	}

	return calls
}
