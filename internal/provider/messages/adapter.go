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
		params.Messages = append(params.Messages, message.ToParam())
	}
	if turn[len(turn)-1].StopReason == anthropic.StopReasonPauseTurn {
		return nil, fmt.Errorf("messages: turn still paused after %d continuations", maxPauseContinuations)
	}
	switch last := turn[len(turn)-1]; last.StopReason {
	case anthropic.StopReasonRefusal:
		return nil, fmt.Errorf("messages: model refused the request (category %q): %s", last.StopDetails.Category, last.StopDetails.Explanation)
	case anthropic.StopReasonMaxTokens:
		return nil, errors.New("messages: response hit max_tokens; raise max_output_tokens in the provider config")
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
				if onDelta != nil && delta.Delta.Text != "" {
					onDelta(delta.Delta.Text)
				}
			}
		}
		if err := message.Accumulate(event); err != nil {
			return anthropic.Message{}, nil, err
		}
	}
	return message, partials, stream.Err()
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
				out = append(out, message.ToParam())
			}
		}
	}
	flush()
	return out, nil
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
		out.Usage.Input += int(u.InputTokens)
		out.Usage.Cached += int(u.CacheReadInputTokens)
		out.Usage.Output += int(u.OutputTokens)
		out.Usage.Total += int(u.InputTokens + u.CacheCreationInputTokens + u.CacheReadInputTokens + u.OutputTokens)
	}
	out.Reasoning = strings.Join(reasoning, "\n")
	opaque, err := json.Marshal(turn)
	if err != nil {
		return nil, fmt.Errorf("messages: encode replay payload: %w", err)
	}
	out.Opaque = opaque
	return out, nil
}
