package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/sandbox"
)

const (
	checkListTimeout = 15 * time.Second
	checkLiveTimeout = 60 * time.Second

	// checkToolName and checkSecretNumber mirror the minimal tool round trip in
	// internal/provider/live_test.go; the fake values never leave this process.
	checkToolName     = "get_secret_number"
	checkSecretNumber = "4217"

	// checkLivePrompt is the Codex --live prompt. Its answer (4217) never
	// appears in it, so a reply that merely quotes the prompt fails the check.
	checkLivePrompt = "What is 2713 plus 1504? Reply with just the number."
)

// checkTool is the only tool offered during a --live check.
var checkTool = provider.ToolSpec{
	Name:        checkToolName,
	Description: "Returns the secret number. Always call this before answering.",
	Parameters:  json.RawMessage(`{"type":"object","properties":{},"required":[]}`),
}

// reportSandbox prints whether the kernel sandbox is usable. It is informational
// and never changes the exit code: the native tools work without Landlock, but
// auto-allowed shell calls then exit 126 instead of running unsandboxed.
func reportSandbox(w io.Writer) {
	if err := sandbox.Available(); err != nil {
		fmt.Fprintf(w, "sandbox  Landlock   WARN: %v; auto-allowed shell calls will exit 126 (Linux 5.13+ required)\n", err)
		return
	}
	fmt.Fprintln(w, "sandbox  Landlock   OK")
}

// runCheck validates every configured provider's auth, endpoint and models,
// optionally performing a minimal tool-call round trip or agent turn per
// reachable provider when live is true. It writes human-readable lines to w,
// never any key value, and returns the process exit code: 1 when any provider
// failed. It also fails when no provider has a key set or passes an auth check.
func runCheck(w io.Writer, cfg *config.Config, live bool) int {
	if live {
		fmt.Fprintln(w, "--live will make real, billed API calls to each configured provider.")
	}

	names := make([]string, 0, len(cfg.Providers))
	for name := range cfg.Providers {
		names = append(names, name)
	}
	sort.Strings(names)

	checked, skipped, failed := 0, 0, 0
	for _, name := range names {
		p := cfg.Providers[name]
		fmt.Fprintf(w, "provider %s (%s)\n", name, p.API)

		if p.API == config.APICodexAppServer {
			authOK, checkFailed := checkCodexProvider(w, name, p, live)
			if authOK {
				checked++
			}
			if checkFailed {
				failed++
			}
			continue
		}

		key := os.Getenv(p.EnvKey)
		if key == "" {
			fmt.Fprintf(w, "  key    %s   not set, skipped\n", p.EnvKey)
			skipped++
			continue
		}
		fmt.Fprintf(w, "  key    %s   set\n", p.EnvKey)
		checked++

		adapter, err := provider.New(name, p, key)
		if err != nil {
			fmt.Fprintf(w, "  api    %s   FAIL: %v\n", p.BaseURL, err)
			failed++
			continue
		}

		lister, listable := adapter.(provider.ModelLister)
		listingOK := false
		if !listable {
			fmt.Fprintf(w, "  api    %s   OK (model listing not supported)\n", p.BaseURL)
		} else {
			ctx, cancel := context.WithTimeout(context.Background(), checkListTimeout)
			ids, err := lister.ListModels(ctx)
			cancel()
			if err != nil {
				fmt.Fprintf(w, "  api    %s   FAIL: %v\n", p.BaseURL, err)
				failed++
				continue
			}
			fmt.Fprintf(w, "  api    %s   OK (%d models listed)\n", p.BaseURL, len(ids))
			listed := make(map[string]bool, len(ids))
			for _, id := range ids {
				listed[id] = true
			}
			for _, id := range p.ModelIDs() {
				if listed[id] {
					fmt.Fprintf(w, "  model  %s     OK\n", id)
				} else {
					fmt.Fprintf(w, "  model  %s     WARN: not in the provider's current model list\n", id)
				}
			}
			listingOK = true
		}

		if !live || (!listingOK && listable) {
			continue
		}
		start := time.Now()
		if err := checkLiveRoundTrip(adapter, p.DefaultModel); err != nil {
			fmt.Fprintf(w, "  live   %s   FAIL: %v\n", p.DefaultModel, err)
			failed++
			continue
		}
		fmt.Fprintf(w, "  live   %s   OK (tool call + follow-up succeeded, %s)\n",
			p.DefaultModel, time.Since(start).Round(time.Millisecond))
	}

	status := "PASS"
	reason := ""
	switch {
	case failed > 0:
		status = "FAIL"
	case checked == 0:
		status = "FAIL"
		reason = " — no configured provider has its key set"
	}
	fmt.Fprintf(w, "result   %s (%d checked, %d skipped, %d failed)%s\n", status, checked, skipped, failed, reason)
	if status == "FAIL" {
		return 1
	}
	return 0
}

// checkCodexProvider returns whether auth passed and whether any check failed.
// The registered Codex adapter implements AuthChecker, ModelLister and Agent.
func checkCodexProvider(w io.Writer, name string, p config.Provider, live bool) (bool, bool) {
	adapter, err := provider.New(name, p, "")
	if err != nil {
		fmt.Fprintf(w, "  auth   FAIL: %v\n", err)
		return false, true
	}

	ctx, cancel := context.WithTimeout(context.Background(), checkListTimeout)
	status, err := adapter.(provider.AuthChecker).CheckAuth(ctx)
	cancel()
	if err != nil {
		fmt.Fprintf(w, "  auth   FAIL: %v\n", err)
		return false, true
	}
	fmt.Fprintf(w, "  auth   %s   OK\n", status.Summary)

	ctx, cancel = context.WithTimeout(context.Background(), checkListTimeout)
	ids, err := adapter.(provider.ModelLister).ListModels(ctx)
	cancel()
	if err != nil {
		fmt.Fprintf(w, "  api    FAIL: %v\n", err)
		return true, true
	}
	fmt.Fprintf(w, "  api    %s   OK (%d models listed)\n", status.Version, len(ids))
	listed := make(map[string]bool, len(ids))
	for _, id := range ids {
		listed[id] = true
	}
	for _, id := range p.ModelIDs() {
		if listed[id] {
			fmt.Fprintf(w, "  model  %s     OK\n", id)
		} else {
			fmt.Fprintf(w, "  model  %s     WARN: not in the provider's current model list\n", id)
		}
	}

	if live {
		start := time.Now()
		if err := checkLiveThread(adapter.(provider.Agent), p.DefaultModel); err != nil {
			fmt.Fprintf(w, "  live   %s   FAIL: %v\n", p.DefaultModel, err)
			return true, true
		}
		fmt.Fprintf(w, "  live   %s   OK (turn succeeded, %s)\n",
			p.DefaultModel, time.Since(start).Round(time.Millisecond))
	}
	return true, false
}

func checkLiveThread(agent provider.Agent, model string) error {
	ctx, cancel := context.WithTimeout(context.Background(), checkLiveTimeout)
	defer cancel()

	thread, err := agent.StartThread(ctx, provider.ThreadOptions{
		Model: model, Cwd: os.TempDir(), Sandbox: "read-only", ApprovalPolicy: "never",
		Ephemeral: true,
	})
	if err != nil {
		return err
	}
	defer thread.Close()
	text, err := thread.Run(ctx, checkLivePrompt, "low", provider.ThreadCallbacks{})
	if err != nil {
		return err
	}
	if !strings.Contains(text, checkSecretNumber) {
		return fmt.Errorf("answer %q does not contain %s", text, checkSecretNumber)
	}
	return nil
}

// checkLiveRoundTrip performs the same minimal tool-call round trip as
// internal/provider/live_test.go: ask, require a tool call, answer the tool,
// then require the follow-up text to contain the tool result.
func checkLiveRoundTrip(p provider.Provider, model string) error {
	ctx, cancel := context.WithTimeout(context.Background(), checkLiveTimeout)
	defer cancel()

	history := []provider.Message{{
		Role: provider.RoleUser,
		Text: "What is the secret number? Use the tool, then answer with just the number.",
	}}
	first, err := p.Turn(ctx, provider.TurnRequest{
		Model:    model,
		Effort:   "low",
		Messages: history,
		Tools:    []provider.ToolSpec{checkTool},
	}, nil)
	if err != nil {
		return err
	}
	if len(first.ToolCalls) == 0 {
		return fmt.Errorf("model did not call the %s tool", checkToolName)
	}

	history = append(history, provider.Message{
		Role:      provider.RoleAssistant,
		Text:      first.Text,
		ToolCalls: first.ToolCalls,
		Opaque:    first.Opaque,
	})
	for _, call := range first.ToolCalls {
		history = append(history, provider.Message{
			Role:       provider.RoleTool,
			ToolCallID: call.ID,
			Text:       checkSecretNumber,
		})
	}

	second, err := p.Turn(ctx, provider.TurnRequest{
		Model:    model,
		Effort:   "low",
		Messages: history,
		Tools:    []provider.ToolSpec{checkTool},
	}, nil)
	if err != nil {
		return fmt.Errorf("follow-up turn: %w", err)
	}
	if !strings.Contains(second.Text, checkSecretNumber) {
		return fmt.Errorf("follow-up answer %q does not contain the tool result", second.Text)
	}
	return nil
}
