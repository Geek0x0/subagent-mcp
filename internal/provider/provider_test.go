package provider_test

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/provider/chatcompletions"
	"github.com/Geek0x0/subagent-mcp/internal/provider/messages"
	"github.com/Geek0x0/subagent-mcp/internal/provider/responses"
)

// Compile-time assertions: each real adapter must expose the optional
// ModelLister capability a config checker uses to validate the provider's
// credentials and model ids.
var (
	_ provider.ModelLister = (*chatcompletions.Adapter)(nil)
	_ provider.ModelLister = (*responses.Adapter)(nil)
	_ provider.ModelLister = (*messages.Adapter)(nil)
)

func TestAdaptersImplementModelLister(t *testing.T) {
	for _, api := range []string{config.APIChatCompletions, config.APIResponses, config.APIMessages} {
		p, err := provider.New("test", config.Provider{API: api, BaseURL: "http://127.0.0.1:1"}, "key")
		if err != nil {
			t.Fatalf("New(%q) error: %v", api, err)
		}
		if _, ok := p.(provider.ModelLister); !ok {
			t.Errorf("%s adapter does not implement provider.ModelLister", api)
		}
	}
}

// TestDecodeToolSchema pins the one contract every adapter shares: a schema must
// be a JSON object, an absent one is a tool with no arguments, and a malformed
// one is an error naming the tool rather than a schema silently replaced by an
// empty one or passed on unchecked.
func TestDecodeToolSchema(t *testing.T) {
	tests := []struct {
		name    string
		spec    provider.ToolSpec
		wantErr string
	}{
		{
			name: "object schema",
			spec: provider.ToolSpec{Name: "shell", Parameters: json.RawMessage(`{"type":"object","properties":{"a":{"type":"string"}}}`)},
		},
		{
			name: "absent schema is an empty object",
			spec: provider.ToolSpec{Name: "ping"},
		},
		{
			name:    "malformed json names the tool",
			spec:    provider.ToolSpec{Name: "shell", Parameters: json.RawMessage(`{`)},
			wantErr: "tool shell schema",
		},
		{
			name:    "array is not an object",
			spec:    provider.ToolSpec{Name: "shell", Parameters: json.RawMessage(`[]`)},
			wantErr: "tool shell schema",
		},
		{
			name:    "json null is not an object",
			spec:    provider.ToolSpec{Name: "shell", Parameters: json.RawMessage(`null`)},
			wantErr: "not a JSON object",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got, err := provider.DecodeToolSchema(test.spec)
			if test.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), test.wantErr) {
					t.Fatalf("DecodeToolSchema() error = %v, want it to contain %q", err, test.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("DecodeToolSchema() error = %v", err)
			}
			if got == nil {
				t.Error("DecodeToolSchema() returned a nil map, want a usable object")
			}
		})
	}
}
