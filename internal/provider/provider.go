// Package provider defines the provider-neutral conversation model used by the
// agent loop and the interface each model API adapter implements.
package provider

import (
	"context"
	"encoding/json"
)

// Role identifies who produced a Message.
type Role string

const (
	// RoleUser is a user prompt.
	RoleUser Role = "user"
	// RoleAssistant is a model reply, optionally carrying tool calls.
	RoleAssistant Role = "assistant"
	// RoleTool is the result of one tool call.
	RoleTool Role = "tool"
)

// ToolSpec describes one tool offered to the model.
type ToolSpec struct {
	Name        string
	Description string
	Parameters  json.RawMessage
}

// ToolCall is one tool invocation requested by the model.
type ToolCall struct {
	ID        string
	Name      string
	Arguments string
}

// Message is one provider-neutral history entry.
type Message struct {
	Role       Role
	Text       string
	ToolCalls  []ToolCall
	ToolCallID string
	IsError    bool
	// Opaque is the provider replay payload; produced and consumed only by the
	// adapter that created it.
	Opaque json.RawMessage
}

// Usage reports token counts for one turn. Every adapter maps its API onto
// the same meaning: Input is the full prompt token count INCLUDING cached
// tokens; Cached is the part of Input served from cache (Cached <= Input);
// Output is the generated completion; Reasoning is the reasoning/thinking
// share of the output when the API reports one; Total is Input + Output.
type Usage struct {
	Input     int
	Cached    int
	Output    int
	Reasoning int
	Total     int
}

// TurnRequest is one model turn request.
type TurnRequest struct {
	Model    string
	Effort   string
	System   string
	Messages []Message
	Tools    []ToolSpec
}

// TurnResult is one model turn reply.
type TurnResult struct {
	Text      string
	Reasoning string
	ToolCalls []ToolCall
	Usage     *Usage
	Opaque    json.RawMessage
}

// Provider is one configured model API adapter.
type Provider interface {
	Name() string
	Turn(ctx context.Context, req TurnRequest, onDelta func(string)) (*TurnResult, error)
}

// ModelLister is an optional capability a Provider MAY additionally implement.
// It lists the model ids the provider's API currently reports, for config
// validation (verifying a base_url/key and model ids are real) rather than for
// use in a Turn. Callers type-assert a Provider to ModelLister to check whether
// the capability is available.
type ModelLister interface {
	ListModels(ctx context.Context) ([]string, error)
}
