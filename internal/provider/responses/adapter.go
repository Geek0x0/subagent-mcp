// Package responses adapts the OpenAI Responses API to provider.Provider.
package responses

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"

	"github.com/openai/openai-go/v3"
	"github.com/openai/openai-go/v3/option"
	"github.com/openai/openai-go/v3/packages/param"
	oresponses "github.com/openai/openai-go/v3/responses"
	"github.com/openai/openai-go/v3/shared"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func init() { provider.Register(config.APIResponses, New) }

// Adapter implements provider.Provider over the OpenAI Responses API.
type Adapter struct {
	name   string
	client openai.Client
}

// New builds a Responses adapter.
func New(name string, cfg config.Provider, apiKey string) (provider.Provider, error) {
	// openai-go has no public opt-out for its OPENAI_* environment
	// defaults, so an allowlist middleware drops every header this adapter
	// did not configure (OPENAI_ORG_ID, OPENAI_PROJECT_ID,
	// OPENAI_CUSTOM_HEADERS, ...) immediately before each attempt is sent.
	// The SDK's default client clones http.DefaultTransport per client, so
	// sessions would never share connections; hand it the pool registered
	// for this base URL instead. The client stays per adapter (and per
	// session key); only the pool is shared.
	client := openai.NewClient(
		option.WithAPIKey(apiKey),
		option.WithAdminAPIKey(""), // never fall back to OPENAI_ADMIN_KEY
		option.WithBaseURL(cfg.BaseURL),
		option.WithMiddleware(keepAllowedHeaders),
		option.WithHTTPClient(provider.PooledClient(provider.KindResponses, cfg.BaseURL, 0)),
	)
	return &Adapter{name: name, client: client}, nil
}

// keepAllowedHeaders reduces the outbound request to the configured Bearer
// credential, the SDK's protocol/content headers, its X-Stainless-*
// telemetry, and the User-Agent; anything else — in particular anything
// derived from the process environment — is removed before the request
// leaves, so no present or future SDK environment default can reach a
// server the user configured for another provider.
func keepAllowedHeaders(req *http.Request, next option.MiddlewareNext) (*http.Response, error) {
	var drop []string
	for name := range req.Header {
		if !allowedResponseHeader(name) {
			drop = append(drop, name)
		}
	}
	for _, name := range drop {
		req.Header.Del(name)
	}
	return next(req)
}

func allowedResponseHeader(name string) bool {
	canonical := http.CanonicalHeaderKey(name)
	switch canonical {
	case "Accept", "Content-Type", "Authorization", "User-Agent":
		return true
	}
	return strings.HasPrefix(canonical, "X-Stainless-")
}

func (a *Adapter) Name() string { return a.name }

// ListModels reports the model ids the configured Responses API currently advertises.
func (a *Adapter) ListModels(ctx context.Context) ([]string, error) {
	pager := a.client.Models.ListAutoPaging(ctx)
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
	input := oresponses.ResponseInputParam{}
	for _, m := range req.Messages {
		switch m.Role {
		case provider.RoleUser:
			input = append(input, oresponses.ResponseInputItemParamOfMessage(m.Text, oresponses.EasyInputMessageRoleUser))
		case provider.RoleAssistant:
			var items []json.RawMessage
			if err := json.Unmarshal(m.Opaque, &items); err != nil {
				return nil, fmt.Errorf("responses: corrupt replay payload: %w", err)
			}
			for _, item := range items {
				input = append(input, param.Override[oresponses.ResponseInputItemUnionParam](item))
			}
		case provider.RoleTool:
			output := oresponses.ResponseInputItemParamOfFunctionCallOutput(m.Text)
			output.OfFunctionCallOutput.CallID = openai.String(m.ToolCallID)
			input = append(input, output)
		}
	}
	tools := make([]oresponses.ToolUnionParam, len(req.Tools))
	for i, spec := range req.Tools {
		var schema map[string]any
		if err := json.Unmarshal(spec.Parameters, &schema); err != nil {
			return nil, fmt.Errorf("responses: tool %s schema: %w", spec.Name, err)
		}
		tools[i] = oresponses.ToolUnionParam{OfFunction: &oresponses.FunctionToolParam{
			Name: spec.Name, Description: openai.String(spec.Description), Parameters: schema, Strict: openai.Bool(false),
		}}
	}
	params := oresponses.ResponseNewParams{
		Model:   req.Model,
		Input:   oresponses.ResponseNewParamsInputUnion{OfInputItemList: input},
		Tools:   tools,
		Store:   openai.Bool(false),
		Include: []oresponses.ResponseIncludable{oresponses.ResponseIncludableReasoningEncryptedContent},
	}
	if req.System != "" {
		params.Instructions = openai.String(req.System)
	}
	if req.Effort != "" {
		params.Reasoning = shared.ReasoningParam{Effort: shared.ReasoningEffort(req.Effort), Summary: shared.ReasoningSummaryAuto}
	}

	stream := a.client.Responses.NewStreaming(ctx, params)
	defer stream.Close()
	var final *oresponses.Response
	for stream.Next() {
		event := stream.Current()
		switch event.Type {
		case "response.output_text.delta":
			if onDelta != nil && event.Delta != "" {
				onDelta(event.Delta)
			}
		case "response.completed", "response.incomplete", "response.failed":
			response := event.Response
			final = &response
		case "error":
			return nil, fmt.Errorf("responses: %s: %s", event.Code, event.Message)
		}
	}
	if err := stream.Err(); err != nil {
		return nil, err
	}
	if final == nil {
		return nil, errors.New("responses: stream ended without a terminal response event")
	}
	switch final.Status {
	case "completed":
	case "incomplete":
		return nil, fmt.Errorf("responses: response incomplete: %s", final.IncompleteDetails.Reason)
	default:
		return nil, fmt.Errorf("responses: response %s: %s", final.Status, final.Error.Message)
	}

	out := &provider.TurnResult{}
	raw := make([]json.RawMessage, 0, len(final.Output))
	var reasoning []string
	for _, item := range final.Output {
		if rawJSON := item.RawJSON(); rawJSON != "" {
			raw = append(raw, json.RawMessage(rawJSON))
		}
		switch item.Type {
		case "message":
			for _, part := range item.Content {
				switch part.Type {
				case "output_text":
					out.Text += part.Text
				case "refusal":
					// A refusal part is a completed response with no text and
					// no tool calls; returning it as an empty success would
					// finish the session with no answer. Fail like the
					// Messages adapter does on a refusal stop reason.
					return nil, fmt.Errorf("responses: model refused: %s", part.AsRefusal().Refusal)
				}
			}
		case "function_call":
			call := item.AsFunctionCall()
			out.ToolCalls = append(out.ToolCalls, provider.ToolCall{ID: call.CallID, Name: call.Name, Arguments: call.Arguments})
		case "reasoning":
			for _, summary := range item.Summary {
				reasoning = append(reasoning, summary.Text)
			}
		}
	}
	out.Reasoning = strings.Join(reasoning, "\n")
	if len(raw) == 0 {
		out.Opaque = json.RawMessage("[]")
	} else {
		opaque, err := json.Marshal(raw)
		if err != nil {
			return nil, fmt.Errorf("responses: encode replay payload: %w", err)
		}
		out.Opaque = opaque
	}
	u := final.Usage
	out.Usage = &provider.Usage{
		Input: int(u.InputTokens), Cached: int(u.InputTokensDetails.CachedTokens),
		Output: int(u.OutputTokens), Reasoning: int(u.OutputTokensDetails.ReasoningTokens), Total: int(u.TotalTokens),
	}
	return out, nil
}
