package codexappserver

import (
	"context"
	"errors"
	"fmt"
	"strings"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func init() { provider.Register(config.APICodexAppServer, New) }

// Adapter delegates whole threads to a shared Codex app-server child.
type Adapter struct {
	name string
	pool *pool
}

var (
	_ provider.Provider    = (*Adapter)(nil)
	_ provider.ModelLister = (*Adapter)(nil)
	_ provider.AuthChecker = (*Adapter)(nil)
	_ provider.Agent       = (*Adapter)(nil)
)

// New builds an adapter. The child starts lazily and uses Codex's own login.
func New(name string, cfg config.Provider, _ string) (provider.Provider, error) {
	return &Adapter{name: name, pool: poolForCommand(cfg.Command)}, nil
}

func (a *Adapter) Name() string { return a.name }

func (a *Adapter) Turn(context.Context, provider.TurnRequest, func(string)) (*provider.TurnResult, error) {
	return nil, errors.New("codex-app-server runs whole threads; Turn is not supported")
}

func (a *Adapter) StartThread(ctx context.Context, opts provider.ThreadOptions) (provider.Thread, error) {
	th, err := startThread(ctx, a.pool, opts)
	if err != nil {
		return nil, err
	}
	return th, nil
}

func (a *Adapter) ListModels(ctx context.Context) ([]string, error) {
	c, err := a.pool.acquire(ctx)
	if err != nil {
		return nil, err
	}
	ids := []string{}
	params := map[string]string{}
	for {
		var result struct {
			Data       []struct{ ID string } `json:"data"`
			NextCursor string                `json:"nextCursor"`
		}
		if err := c.client.Call(ctx, "model/list", params, &result); err != nil {
			return nil, c.failure(err)
		}
		for _, model := range result.Data {
			ids = append(ids, model.ID)
		}
		if result.NextCursor == "" {
			return ids, nil
		}
		params["cursor"] = result.NextCursor
	}
}

func (a *Adapter) CheckAuth(ctx context.Context) (provider.AuthStatus, error) {
	c, err := a.pool.acquire(ctx)
	if err != nil {
		return provider.AuthStatus{}, err
	}
	var status provider.AuthStatus
	if _, version, ok := strings.Cut(c.userAgent, "/"); ok {
		version, _, _ = strings.Cut(version, " ")
		if version != "" {
			status.Version = "codex-cli " + version
		}
	}
	var result struct {
		Account *struct {
			Type     string `json:"type"`
			PlanType string `json:"planType"`
		} `json:"account"`
		RequiresOpenAIAuth bool `json:"requiresOpenaiAuth"`
	}
	if err := c.client.Call(ctx, "account/read", struct{}{}, &result); err != nil {
		return status, c.failure(err)
	}
	if result.RequiresOpenAIAuth && result.Account == nil {
		return status, errors.New("not logged in; run codex login")
	}
	if result.Account != nil {
		status.Summary = fmt.Sprintf("%s (%s)", result.Account.Type, result.Account.PlanType)
	}
	return status, nil
}
