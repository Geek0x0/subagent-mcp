// Package messages adapts the Anthropic Messages API to provider.Provider.
package messages

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"time"

	"github.com/anthropics/anthropic-sdk-go"
	"github.com/anthropics/anthropic-sdk-go/option"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func init() { provider.Register(config.APIMessages, New) }

// maxPauseContinuations bounds pause_turn re-requests within one turn.
const maxPauseContinuations = 5

// Adapter implements provider.Provider over the Anthropic Messages API.
type Adapter struct {
	name      string
	client    anthropic.Client
	maxTokens int64
}

// New builds a Messages adapter.
func New(name string, cfg config.Provider, apiKey string) (provider.Provider, error) {
	// WithoutEnvironmentDefaults stops the SDK from autoloading its
	// credential environment (ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN,
	// ANTHROPIC_CUSTOM_HEADERS, ANTHROPIC_BASE_URL, profiles, federation):
	// the only credential on the wire is the configured key below, no
	// matter what the process environment holds. The transport is the pool
	// shared for this base URL (sessions reuse connections); the client
	// itself stays per adapter, carrying this session's key.
	client := anthropic.NewClient(
		option.WithoutEnvironmentDefaults(),
		option.WithHTTPClient(&http.Client{
			Transport: provider.SharedTransport(provider.KindMessages, cfg.BaseURL, responseHeaderTimeout),
		}),
		option.WithAPIKey(apiKey),
		option.WithBaseURL(cfg.BaseURL),
	)
	return &Adapter{name: name, client: client, maxTokens: int64(cfg.MaxOutputTokens)}, nil
}

// responseHeaderTimeout matches the SDK's internal default client, which
// option.WithoutEnvironmentDefaults skips along with the environment
// autoload: bound the wait for response headers so a server that accepts
// the connection but never responds cannot hang a request forever. The
// timeout does not apply to the response body, so streams are unaffected.
const responseHeaderTimeout = 10 * time.Minute

func (a *Adapter) Name() string { return a.name }

// ListModels reports the model ids the configured Messages API currently advertises.
func (a *Adapter) ListModels(ctx context.Context) ([]string, error) {
	pager := a.client.Models.ListAutoPaging(ctx, anthropic.ModelListParams{})
	var ids []string
	for pager.Next() {
		ids = append(ids, pager.Current().ID)
	}
	if err := pager.Err(); err != nil {
		return nil, fmt.Errorf("list models: %w", err)
	}
	return ids, nil
}

func (a *Adapter) Turn(ctx context.Context, req provider.TurnRequest, onDelta func(string)) (*provider.TurnResult, error) {
	messages, err := buildMessages(req.Messages)
	if err != nil {
		return nil, err
	}
	params := anthropic.MessageNewParams{
		Model:        anthropic.Model(req.Model),
		MaxTokens:    a.maxTokens,
		Messages:     messages,
		Tools:        buildTools(req.Tools),
		Thinking:     anthropic.ThinkingConfigParamUnion{OfAdaptive: &anthropic.ThinkingConfigAdaptiveParam{Display: anthropic.ThinkingConfigAdaptiveDisplaySummarized}},
		CacheControl: anthropic.NewCacheControlEphemeralParam(),
	}
	if req.System != "" {
		params.System = []anthropic.TextBlockParam{{Text: req.System}}
	}
	if req.Effort != "" {
		params.OutputConfig = anthropic.OutputConfigParam{Effort: anthropic.OutputConfigEffort(req.Effort)}
	}

	var turn []anthropic.Message
	// partials mirrors turn: one index → raw input JSON map per streamed message.
	var partials []map[int64]string
	for attempt := 0; ; attempt++ {
		message, blockInput, err := a.stream(ctx, params, onDelta)
		if err != nil {
			return nil, err
		}
		turn = append(turn, message)
		partials = append(partials, blockInput)
		if message.StopReason != anthropic.StopReasonPauseTurn || attempt >= maxPauseContinuations {
			break
		}
		if param, ok := replayMessage(message); ok {
			params.Messages = append(params.Messages, param)
		}
	}
	if turn[len(turn)-1].StopReason == anthropic.StopReasonPauseTurn {
		return nil, fmt.Errorf("messages: turn still paused after %d continuations", maxPauseContinuations)
	}
	switch last := turn[len(turn)-1]; last.StopReason {
	case anthropic.StopReasonRefusal:
		return nil, fmt.Errorf("messages: model refused the request (category %q): %s", last.StopDetails.Category, last.StopDetails.Explanation)
	case anthropic.StopReasonMaxTokens:
		return nil, errors.New("messages: response hit max_tokens; raise max_output_tokens in the provider config")
	case anthropic.StopReasonModelContextWindowExceeded:
		return nil, errors.New("messages: response hit model_context_window_exceeded; shorten the conversation or use a model with a larger context window")
	}
	return toResult(turn, partials)
}

// stream accumulates one message from the SSE stream. Alongside the SDK
// accumulator it records each tool_use block's raw input JSON per block index:
// the accumulator drops a truncated input_json_delta at content_block_stop, and
// the runner needs that text verbatim to reject the call as invalid.
func (a *Adapter) stream(ctx context.Context, params anthropic.MessageNewParams, onDelta func(string)) (anthropic.Message, map[int64]string, error) {
	stream := a.client.Messages.NewStreaming(ctx, params)
	defer stream.Close()
	message := anthropic.Message{}
	partials := map[int64]string{}
	var streamed strings.Builder
	for stream.Next() {
		event := stream.Current()
		switch event.Type {
		case "content_block_start":
			block := event.AsContentBlockStart()
			if block.ContentBlock.Type == "tool_use" {
				if raw, err := json.Marshal(block.ContentBlock.Input); err == nil && string(raw) != "null" {
					partials[block.Index] = string(raw)
				}
			}
		case "content_block_delta":
			delta := event.AsContentBlockDelta()
			switch delta.Delta.Type {
			case "input_json_delta":
				// Mirrors the SDK accumulator: a first delta replaces the "{}"
				// placeholder, later deltas append.
				if current, ok := partials[delta.Index]; ok && current != "{}" {
					partials[delta.Index] = current + delta.Delta.PartialJSON
				} else {
					partials[delta.Index] = delta.Delta.PartialJSON
				}
			case "text_delta":
				if delta.Delta.Text != "" {
					streamed.WriteString(delta.Delta.Text)
					if onDelta != nil {
						onDelta(delta.Delta.Text)
					}
				}
			}
		}
		if err := message.Accumulate(event); err != nil {
			return anthropic.Message{}, nil, err
		}
	}
	if err := stream.Err(); err != nil {
		return anthropic.Message{}, nil, err
	}
	// A stream that ends before message_delta carries no stop signal: it is a
	// truncated reply, not an answer. The block JSON is refreshed only at
	// block/message stop, so success here would report empty text even though
	// deltas were streamed, and store a block the API rejects on replay.
	if message.StopReason == "" {
		return anthropic.Message{}, nil, fmt.Errorf(
			"messages: stream ended without a terminal stop signal (partial text: %q)",
			truncateRunes(streamed.String(), 120))
	}
	return message, partials, nil
}

// buildMessages replays neutral history; consecutive tool results become one user message.
func buildMessages(history []provider.Message) ([]anthropic.MessageParam, error) {
	var out []anthropic.MessageParam
	var results []anthropic.ContentBlockParamUnion
	flush := func() {
		if len(results) > 0 {
			out = append(out, anthropic.NewUserMessage(results...))
			results = nil
		}
	}
	for _, m := range history {
		switch m.Role {
		case provider.RoleTool:
			results = append(results, anthropic.NewToolResultBlock(m.ToolCallID, m.Text, m.IsError))
			continue
		case provider.RoleUser:
			flush()
			out = append(out, anthropic.NewUserMessage(anthropic.NewTextBlock(m.Text)))
		case provider.RoleAssistant:
			flush()
			var turn []anthropic.Message
			if err := json.Unmarshal(m.Opaque, &turn); err != nil {
				return nil, fmt.Errorf("messages: corrupt replay payload: %w", err)
			}
			for _, message := range turn {
				if param, ok := replayMessage(message); ok {
					out = append(out, param)
				}
			}
		}
	}
	flush()
	return out, nil
}

// replayMessage converts one stored assistant message into a request message,
// dropping empty text blocks. A message with nothing sendable left — an empty
// content array from a legal empty end_turn reply, or only empty text blocks —
// is not replayed at all: the Messages API rejects any message whose content
// is empty, so replaying one would 400 every later turn on the session.
func replayMessage(message anthropic.Message) (anthropic.MessageParam, bool) {
	blocks := make([]anthropic.ContentBlockParamUnion, 0, len(message.Content))
	for _, block := range message.Content {
		if text, ok := block.AsAny().(anthropic.TextBlock); ok && text.Text == "" {
			continue
		}
		blocks = append(blocks, block.ToParam())
	}
	if len(blocks) == 0 {
		return anthropic.MessageParam{}, false
	}
	return anthropic.MessageParam{Role: anthropic.MessageParamRole(message.Role), Content: blocks}, true
}

// truncateRunes bounds text embedded in an error message.
func truncateRunes(s string, max int) string {
	if runes := []rune(s); len(runes) > max {
		return string(runes[:max]) + "…"
	}
	return s
}

func buildTools(specs []provider.ToolSpec) []anthropic.ToolUnionParam {
	tools := make([]anthropic.ToolUnionParam, len(specs))
	for i, spec := range specs {
		var schema struct {
			Properties any      `json:"properties"`
			Required   []string `json:"required"`
		}
		_ = json.Unmarshal(spec.Parameters, &schema)
		tool := anthropic.ToolParam{
			Name:                spec.Name,
			Description:         anthropic.String(spec.Description),
			InputSchema:         anthropic.ToolInputSchemaParam{Properties: schema.Properties, Required: schema.Required},
			EagerInputStreaming: anthropic.Bool(true),
		}
		tools[i] = anthropic.ToolUnionParam{OfTool: &tool}
	}
	return tools
}

func toResult(turn []anthropic.Message, partials []map[int64]string) (*provider.TurnResult, error) {
	out := &provider.TurnResult{Usage: &provider.Usage{}}
	var reasoning []string
	for mi, message := range turn {
		for bi, block := range message.Content {
			switch b := block.AsAny().(type) {
			case anthropic.TextBlock:
				out.Text += b.Text
			case anthropic.ThinkingBlock:
				if b.Thinking != "" {
					reasoning = append(reasoning, b.Thinking)
				}
			case anthropic.ToolUseBlock:
				arguments := string(b.Input)
				if partial, ok := partials[mi][int64(bi)]; ok && partial != "" && partial != "{}" {
					arguments = partial
				}
				out.ToolCalls = append(out.ToolCalls, provider.ToolCall{ID: b.ID, Name: b.Name, Arguments: arguments})
			}
		}
		u := message.Usage
		input := int(u.InputTokens + u.CacheCreationInputTokens + u.CacheReadInputTokens)
		out.Usage.Input += input
		out.Usage.Cached += int(u.CacheReadInputTokens)
		out.Usage.Output += int(u.OutputTokens)
		out.Usage.Reasoning += int(u.OutputTokensDetails.ThinkingTokens)
		out.Usage.Total += input + int(u.OutputTokens)
	}
	out.Reasoning = strings.Join(reasoning, "\n")
	opaque, err := json.Marshal(replayTurn(turn))
	if err != nil {
		return nil, fmt.Errorf("messages: encode replay payload: %w", err)
	}
	out.Opaque = opaque
	return out, nil
}

// replayTurn drops empty text blocks from the stored turn: such a block can
// only come from a stream cut before its content_block_stop, and the API
// rejects it on replay.
func replayTurn(turn []anthropic.Message) []anthropic.Message {
	stored := make([]anthropic.Message, len(turn))
	for i, message := range turn {
		blocks := make([]anthropic.ContentBlockUnion, 0, len(message.Content))
		for _, block := range message.Content {
			if text, ok := block.AsAny().(anthropic.TextBlock); ok && text.Text == "" {
				continue
			}
			blocks = append(blocks, block)
		}
		message.Content = blocks
		stored[i] = message
	}
	return stored
}
