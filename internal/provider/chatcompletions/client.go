package chatcompletions

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
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
	// Upstream is the channel the gateway routed the call to, when it says.
	Upstream string
}

type Client struct {
	oai     *openai.Client
	Backoff func(attempt int) time.Duration
}

// NewClient builds a low-level Chat Completions client.
func NewClient(apiKey, baseURL string) *Client { return NewClientWithBody(apiKey, baseURL, nil) }

// NewClientWithBody is NewClient that also merges extraBody (fields the SDK
// request type has no place for, such as a gateway's upstream-routing
// preferences) into every request body.
func NewClientWithBody(apiKey, baseURL string, extraBody map[string]any) *Client {
	cfg := openai.DefaultConfig(apiKey)
	cfg.BaseURL = baseURL
	// go-openai's DefaultConfig gives every client an &http.Client{} whose
	// nil Transport resolves to the process-global http.DefaultTransport at
	// request time. Bind the client to the pool registered for this base URL
	// instead: sessions against one endpoint reuse its connections without
	// coupling the endpoint (or other endpoints) to the process-wide default.
	// The client itself stays per adapter, carrying this session's key.
	cfg.HTTPClient = provider.PooledClientWithBody(provider.KindChatCompletions, baseURL, 0, extraBody)

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
	ctx, upstream := provider.WithUpstreamRecorder(ctx)

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
	var finishReason string
	assembler := newToolCallAssembler()
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
			// The finish reason can ride on any chunk: some servers put it on
			// an otherwise-empty delta after the last content delta, and the
			// usage chunk (empty choices) comes after it. Keep the last
			// non-empty reason the stream reports.
			if choice.FinishReason != "" {
				finishReason = string(choice.FinishReason)
			}

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

			for _, entry := range delta.ToolCalls {
				assembler.add(entry)
			}
		}
	}

	if err := finishReasonError(finishReason); err != nil {
		return nil, err
	}
	toolCalls, err := assembler.result()
	if err != nil {
		return nil, err
	}

	return &TurnResult{
		Content:   content.String(),
		Reasoning: reasoning.String(),
		ToolCalls: toolCalls,
		Usage:     usage,
		Upstream:  upstream(),
	}, nil
}

// finishReasonError maps the stream's finish_reason to a turn error. "stop" and
// "tool_calls" (and the aliases other OpenAI-compatible servers use for the same
// thing: "function_call", "eos") mean the model finished cleanly. "length" and
// "content_filter" are truncated or filtered answers, and no finish reason at all
// means the transport ended mid-stream (go-openai reports both a clean [DONE] and
// a body that just ends as io.EOF, so the loop alone cannot tell them apart).
// A reason this adapter does not know is logged and accepted: the server did say
// the stream finished, and failing every turn on a compatible server's private
// vocabulary would make it unusable.
func finishReasonError(reason string) error {
	switch reason {
	case string(openai.FinishReasonStop), string(openai.FinishReasonToolCalls),
		string(openai.FinishReasonFunctionCall), "eos":
		return nil
	case string(openai.FinishReasonLength), "model_length":
		return errors.New(`chat completions: finish_reason "length": response was truncated by the output token limit`)
	case string(openai.FinishReasonContentFilter):
		return errors.New(`chat completions: finish_reason "content_filter": response was stopped by the content filter`)
	case "":
		return errors.New("chat completions: stream ended without a finish reason (truncated response)")
	default:
		log.Printf("chat completions: unrecognised finish_reason %q; treating the turn as finished", reason)
		return nil
	}
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

// toolCallAssembler rebuilds the tool calls the server sent from the streamed
// fragments. A fragment carrying an index continues the call with that index.
// A fragment without an index starts a new call when it carries an id the
// current call does not already have, and otherwise continues the most recent
// call: arguments append, and an id, name or type that earlier fragments left
// empty gets filled in (some servers send only the index, or only the id and
// name, in the first fragment).
type toolCallAssembler struct {
	calls    []assembledCall
	indexPos map[int]int // server index -> position in calls
	last     int         // position of the most recently touched call, -1 if none
}

type assembledCall struct {
	call  openai.ToolCall
	index int // server-supplied index; -1 when the server omitted it
}

func newToolCallAssembler() *toolCallAssembler {
	return &toolCallAssembler{indexPos: make(map[int]int), last: -1}
}

func (a *toolCallAssembler) add(entry openai.ToolCall) {
	pos := a.positionFor(entry)
	a.last = pos

	call := &a.calls[pos].call
	if entry.ID != "" && call.ID == "" {
		call.ID = entry.ID
	}
	if entry.Type != "" && call.Type == "" {
		call.Type = entry.Type
	}
	if entry.Function.Name != "" && call.Function.Name == "" {
		call.Function.Name = entry.Function.Name
	}
	call.Function.Arguments += entry.Function.Arguments
}

func (a *toolCallAssembler) positionFor(entry openai.ToolCall) int {
	if entry.Index != nil {
		if pos, ok := a.indexPos[*entry.Index]; ok {
			return pos
		}
		return a.append(*entry.Index)
	}

	if a.last < 0 {
		return a.append(-1)
	}
	// No index: a new non-empty id that the most recent call does not already
	// carry starts the next call; anything else continues that call.
	if entry.ID != "" && a.calls[a.last].call.ID != "" && a.calls[a.last].call.ID != entry.ID {
		return a.append(-1)
	}
	return a.last
}

func (a *toolCallAssembler) append(index int) int {
	pos := len(a.calls)
	a.calls = append(a.calls, assembledCall{index: index})
	if index >= 0 {
		a.indexPos[index] = pos
	}

	return pos
}

// result returns the assembled calls in the order the server sent them,
// sorted by index when every call carries one (the historical behavior). A
// call the stream never gave an id or a name fails the turn: the agent could
// not address a tool result for it, and the API rejects an empty
// tool_call_id.
func (a *toolCallAssembler) result() ([]openai.ToolCall, error) {
	if len(a.calls) == 0 {
		return nil, nil
	}

	allIndexed := true
	for _, assembled := range a.calls {
		if assembled.index < 0 {
			allIndexed = false
			break
		}
	}

	order := make([]int, len(a.calls))
	for i := range order {
		order[i] = i
	}
	if allIndexed {
		sort.Slice(order, func(i, j int) bool {
			return a.calls[order[i]].index < a.calls[order[j]].index
		})
	}

	calls := make([]openai.ToolCall, 0, len(a.calls))
	for _, pos := range order {
		call := a.calls[pos].call
		if call.ID == "" {
			return nil, fmt.Errorf("chat completions: tool call %d has no id (name %q)", len(calls), call.Function.Name)
		}
		if call.Function.Name == "" {
			return nil, fmt.Errorf("chat completions: tool call %q has no name", call.ID)
		}
		if call.Type == "" {
			call.Type = openai.ToolTypeFunction
		}
		calls = append(calls, call)
	}

	return calls, nil
}
