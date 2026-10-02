package chatcompletions

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	openai "github.com/sashabaranov/go-openai"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func init() { provider.Register(config.APIChatCompletions, New) }

// Adapter implements provider.Provider over OpenAI Chat Completions.
type Adapter struct {
	name   string
	client *Client
}

// New builds a chat-completions adapter.
func New(name string, cfg config.Provider, apiKey string) (provider.Provider, error) {
	return &Adapter{name: name, client: NewClientWithBody(apiKey, cfg.BaseURL, cfg.ExtraBody)}, nil
}

func (a *Adapter) Name() string { return a.name }

// ListModels reports the model ids the configured Chat Completions API currently advertises.
func (a *Adapter) ListModels(ctx context.Context) ([]string, error) {
	models, err := a.client.oai.ListModels(ctx)
	if err != nil {
		return nil, fmt.Errorf("list models: %w", err)
	}
	ids := make([]string, 0, len(models.Models))
	for _, model := range models.Models {
		ids = append(ids, model.ID)
	}
	return ids, nil
}

// SetBackoff overrides the stream-setup retry delay; tests use it to avoid real sleeps.
func (a *Adapter) SetBackoff(backoff func(attempt int) time.Duration) { a.client.Backoff = backoff }

type opaque struct {
	ReasoningContent string `json:"reasoning_content,omitempty"`
}

func (a *Adapter) Turn(ctx context.Context, req provider.TurnRequest, onDelta func(string)) (*provider.TurnResult, error) {
	var messages []openai.ChatCompletionMessage
	if req.System != "" {
		messages = append(messages, openai.ChatCompletionMessage{Role: openai.ChatMessageRoleSystem, Content: req.System})
	}
	for _, m := range req.Messages {
		switch m.Role {
		case provider.RoleUser:
			messages = append(messages, openai.ChatCompletionMessage{Role: openai.ChatMessageRoleUser, Content: m.Text})
		case provider.RoleAssistant:
			msg := openai.ChatCompletionMessage{Role: openai.ChatMessageRoleAssistant, Content: m.Text}
			var o opaque
			if len(m.Opaque) > 0 {
				_ = json.Unmarshal(m.Opaque, &o)
			}
			msg.ReasoningContent = o.ReasoningContent
			for _, call := range m.ToolCalls {
				msg.ToolCalls = append(msg.ToolCalls, openai.ToolCall{
					ID: call.ID, Type: openai.ToolTypeFunction,
					Function: openai.FunctionCall{Name: call.Name, Arguments: call.Arguments},
				})
			}
			messages = append(messages, msg)
		case provider.RoleTool:
			// Chat Completions has no tool-error flag; errors are carried in the content text.
			messages = append(messages, openai.ChatCompletionMessage{Role: openai.ChatMessageRoleTool, ToolCallID: m.ToolCallID, Content: m.Text})
		}
	}
	tools := make([]openai.Tool, len(req.Tools))
	for i, spec := range req.Tools {
		tools[i] = openai.Tool{Type: openai.ToolTypeFunction, Function: &openai.FunctionDefinition{
			Name: spec.Name, Description: spec.Description, Parameters: spec.Parameters,
		}}
	}
	res, err := a.client.ChatTurn(ctx, openai.ChatCompletionRequest{
		Model: req.Model, Messages: messages, Tools: tools, ReasoningEffort: req.Effort,
	}, onDelta)
	if err != nil {
		return nil, err
	}
	out := &provider.TurnResult{Text: res.Content, Reasoning: res.Reasoning, Upstream: res.Upstream}
	for _, call := range res.ToolCalls {
		out.ToolCalls = append(out.ToolCalls, provider.ToolCall{ID: call.ID, Name: call.Function.Name, Arguments: call.Function.Arguments})
	}
	if strings.TrimSpace(res.Reasoning) != "" {
		out.Opaque, _ = json.Marshal(opaque{ReasoningContent: res.Reasoning})
	}
	if u := res.Usage; u != nil {
		usage := &provider.Usage{Input: u.PromptTokens, Output: u.CompletionTokens, Total: u.TotalTokens}
		if u.PromptTokensDetails != nil {
			usage.Cached = u.PromptTokensDetails.CachedTokens
		}
		if u.CompletionTokensDetails != nil {
			usage.Reasoning = u.CompletionTokensDetails.ReasoningTokens
		}
		out.Usage = usage
	}
	return out, nil
}
