package agent

import (
	"context"
	"strings"
	"testing"

	"github.com/Geek0x0/subagent-mcp/internal/provider"
)

func textTurn(text string) stubTurn { return stubTurn{result: &provider.TurnResult{Text: text}} }

func toolTurn(id string) stubTurn {
	return stubTurn{result: &provider.TurnResult{ToolCalls: []provider.ToolCall{toolCall(id, "shell", `{"command":"true"}`)}}}
}

func runNudge(t *testing.T, maxNudges int, turns ...stubTurn) (string, error, *stubProvider, *Session) {
	t.Helper()
	client := &stubProvider{turns: turns}
	session := newTestSession(t, Options{Provider: client, MaxNudges: maxNudges})
	runner := &Runner{Emitter: &recEmitter{}, Approver: &stubApprover{}}
	text, err := runner.Run(context.Background(), session, "task")
	return text, err, client, session
}

func lastRoleText(req provider.TurnRequest) (provider.Role, string) {
	last := req.Messages[len(req.Messages)-1]
	return last.Role, last.Text
}

// A model that narrates its next step and stops is asked to continue, and the
// run carries on through the tool call it makes next.
func TestNudgeContinuesAfterNarrationOnlyReply(t *testing.T) {
	text, err, client, _ := runNudge(t, 2,
		toolTurn("c1"),
		textTurn("Step 2: write the failing test first."),
		toolTurn("c2"),
		textTurn("All done: changed a.go."),
		textTurn("All done: changed a.go."),
	)
	if err != nil || text != "All done: changed a.go." {
		t.Fatalf("Run() = (%q, %v)", text, err)
	}
	if len(client.requests) != 5 {
		t.Fatalf("model calls = %d, want 5", len(client.requests))
	}
	role, got := lastRoleText(client.requests[2])
	if role != provider.RoleUser || got != continuePrompt {
		t.Fatalf("request after narration ends with %v %q, want the continue prompt", role, got)
	}
}

// Nudging is off by default: a reply without a tool call is final.
func TestNudgeDisabledByDefault(t *testing.T) {
	text, err, client, _ := runNudge(t, 0, textTurn("Step 2: write the failing test first."))
	if err != nil || text != "Step 2: write the failing test first." || len(client.requests) != 1 {
		t.Fatalf("Run() = (%q, %v) after %d calls, want the narration returned at once", text, err, len(client.requests))
	}
}

// After one nudge, a second reply without a tool call is accepted: a model that
// was really finished repeats its summary and the run ends.
func TestNudgeAcceptsSecondTextOnlyReplyAsFinal(t *testing.T) {
	text, err, client, _ := runNudge(t, 5,
		textTurn("Finished."),
		textTurn("Finished: nothing else to do."),
	)
	if err != nil || text != "Finished: nothing else to do." || len(client.requests) != 2 {
		t.Fatalf("Run() = (%q, %v) after %d calls, want the second reply as the answer", text, err, len(client.requests))
	}
}

// The nudge budget is per run: once spent, a narration is returned as the answer.
func TestNudgeBudgetIsPerRun(t *testing.T) {
	text, err, client, _ := runNudge(t, 1,
		textTurn("Step 1."),
		toolTurn("c1"),
		textTurn("Step 2."),
	)
	if err != nil || text != "Step 2." || len(client.requests) != 3 {
		t.Fatalf("Run() = (%q, %v) after %d calls, want the second narration returned", text, err, len(client.requests))
	}
}

// An empty reply is nudged too, is not replayed as an empty assistant message,
// and an earlier non-empty text is returned if the model then stays silent.
func TestNudgeHandlesEmptyReplies(t *testing.T) {
	text, err, client, session := runNudge(t, 2,
		textTurn("Summary of the work."),
		textTurn(""),
	)
	if err != nil || text != "Summary of the work." {
		t.Fatalf("Run() = (%q, %v), want the earlier summary", text, err)
	}
	for _, message := range session.messages {
		if message.Role == provider.RoleAssistant && message.Text == "" && len(message.ToolCalls) == 0 {
			t.Fatalf("history holds an empty assistant message: %+v", session.messages)
		}
	}
	if len(client.requests) != 2 {
		t.Fatalf("model calls = %d, want 2", len(client.requests))
	}

	text, err, _, session = runNudge(t, 2, textTurn(""), toolTurn("c1"), textTurn("Done."), textTurn("Done."))
	if err != nil || text != "Done." {
		t.Fatalf("Run() = (%q, %v), want the final summary after the nudged empty reply", text, err)
	}
	for _, message := range session.messages {
		if message.Role == provider.RoleAssistant && message.Text == "" && len(message.ToolCalls) == 0 {
			t.Fatalf("history holds an empty assistant message: %+v", session.messages)
		}
	}
}

// The nudge is announced to event consumers and kept out of the final answer.
func TestNudgeEmitsAnEvent(t *testing.T) {
	client := &stubProvider{turns: []stubTurn{textTurn("Step 1."), textTurn("Step 1 done.")}}
	session := newTestSession(t, Options{Provider: client, MaxNudges: 1})
	emitter := &recEmitter{}
	runner := &Runner{Emitter: emitter, Approver: &stubApprover{}}
	if _, err := runner.Run(context.Background(), session, "task"); err != nil {
		t.Fatal(err)
	}
	var nudges int
	emitter.mu.Lock()
	for _, event := range emitter.events {
		if event["type"] == "agent_nudge" {
			nudges++
			if !strings.Contains(event["message"].(string), "without a tool call") {
				t.Errorf("agent_nudge message = %q", event["message"])
			}
		}
	}
	emitter.mu.Unlock()
	if nudges != 1 {
		t.Fatalf("agent_nudge events = %d, want 1", nudges)
	}
}
