package server

import (
	"context"
	"fmt"
	"strings"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/decision"
	"github.com/Geek0x0/subagent-mcp/internal/provider"

	"github.com/mark3labs/mcp-go/mcp"
)

// autoSelection is one resolved provider/model choice made by Jev, together
// with the adapter built for it.
type autoSelection struct {
	providerName string
	providerCfg  config.Provider
	adapter      provider.Provider
	model        string
	selection    decision.Selection
}

func (a *autoSelection) meta() map[string]any {
	meta := map[string]any{
		"judge_model":   a.selection.JudgeModel,
		"confidence":    a.selection.Confidence,
		"input_tokens":  a.selection.InputTokens,
		"output_tokens": a.selection.OutputTokens,
	}
	if len(a.selection.Probabilities) > 0 {
		meta["probabilities"] = a.selection.Probabilities
	}
	return meta
}

// resolveAuto chooses a configured provider/model with the decision model and
// builds its adapter. A failed decision is a tool error, never a fallback.
func (s *Server) resolveAuto(ctx context.Context, prompt, cwd, providerArg string) (*autoSelection, *mcp.CallToolResult) {
	if providerArg != "" {
		return nil, mcp.NewToolResultError(`provider must be omitted when model is "auto"`)
	}
	if s.cfg.Auto == nil {
		return nil, mcp.NewToolResultError(fmt.Sprintf("model \"auto\" requires an [auto] section in %s", s.cfg.Path))
	}

	candidates, omitted := s.autoCandidates()
	if len(candidates) == 0 {
		return nil, mcp.NewToolResultError(fmt.Sprintf(
			"auto model selection: no provider is usable: %s",
			strings.Join(omitted, "; "),
		))
	}

	var selection decision.Selection
	if len(candidates) == 1 {
		selection = decision.Selection{
			Provider:   candidates[0].Provider,
			Model:      candidates[0].Model,
			Confidence: 1,
		}
	} else {
		key, err := s.cfg.AutoAPIKey()
		if err != nil {
			return nil, mcp.NewToolResultError("auto model selection: " + err.Error())
		}
		client := decision.New(*s.cfg.Auto, key)
		selection, err = client.Choose(ctx, prompt, cwd, candidates)
		if err != nil {
			return nil, mcp.NewToolResultError("auto model selection: " + err.Error())
		}
	}

	providerCfg, err := s.cfg.Provider(selection.Provider)
	if err != nil {
		return nil, mcp.NewToolResultError("auto model selection: " + err.Error())
	}
	if !providerCfg.HasModel(selection.Model) {
		return nil, mcp.NewToolResultError(fmt.Sprintf(
			"auto model selection: chose unknown model %q for provider %q",
			selection.Model,
			selection.Provider,
		))
	}
	key, err := s.cfg.APIKeyFor(selection.Provider)
	if err != nil {
		return nil, mcp.NewToolResultError("auto model selection: " + err.Error())
	}
	adapter, err := provider.New(selection.Provider, providerCfg, key)
	if err != nil {
		return nil, mcp.NewToolResultError("auto model selection: " + err.Error())
	}

	return &autoSelection{
		providerName: selection.Provider,
		providerCfg:  providerCfg,
		adapter:      adapter,
		model:        selection.Model,
		selection:    selection,
	}, nil
}

// autoCandidates lists the models of every provider that can start a session.
// API providers without their key set are omitted with all their models;
// Codex providers authenticate out of band and are always included.
func (s *Server) autoCandidates() ([]decision.Candidate, []string) {
	var candidates []decision.Candidate
	var omitted []string
	for _, name := range s.cfg.ProviderNames() {
		p := s.cfg.Providers[name]
		if p.API != config.APICodexAppServer {
			if _, err := s.cfg.APIKeyFor(name); err != nil {
				omitted = append(omitted, fmt.Sprintf("%s: %v", name, err))
				continue
			}
		}
		for _, model := range p.Models {
			candidates = append(candidates, decision.Candidate{
				Provider:    name,
				Model:       model.ID,
				API:         p.API,
				Description: model.Description,
			})
		}
	}
	return candidates, omitted
}
