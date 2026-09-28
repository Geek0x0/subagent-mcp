package main

import (
	"bytes"
	"context"
	"errors"
	"net/http"
	"os"
	"os/exec"
	"reflect"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
	"github.com/Geek0x0/subagent-mcp/internal/provider"
	"github.com/Geek0x0/subagent-mcp/internal/testutil"
)

func TestMain(m *testing.M) {
	testutil.MaybeRunFakeCodexAppServer()
	// Preserve existing race options, but avoid the exit sleep in fake children.
	if err := os.Setenv("GORACE", strings.TrimSpace(os.Getenv("GORACE")+" atexit_sleep_ms=0")); err != nil {
		panic(err)
	}
	os.Exit(m.Run())
}

const (
	checkKeyA = "SUBAGENT_CHECK_KEY_A"
	checkKeyB = "SUBAGENT_CHECK_KEY_B"

	checkKeyValueA = "sk-check-secret-a-6b1f9c"
	checkKeyValueB = "sk-check-secret-b-2d7e4a"
)

func init() {
	provider.Register("check-no-lister", func(name string, _ config.Provider, _ string) (provider.Provider, error) {
		return &checkStubProvider{name: name}, nil
	})
}

// checkStubProvider is a Provider without the optional ModelLister capability.
type checkStubProvider struct {
	name  string
	calls int
}

func (p *checkStubProvider) Name() string { return p.name }

func (p *checkStubProvider) Turn(context.Context, provider.TurnRequest, func(string)) (*provider.TurnResult, error) {
	p.calls++
	if p.calls == 1 {
		return &provider.TurnResult{ToolCalls: []provider.ToolCall{{ID: "call_1", Name: "get_secret_number", Arguments: "{}"}}}, nil
	}
	return &provider.TurnResult{Text: "The secret number is 4217."}, nil
}

func chatProvider(baseURL, envKey, defaultModel string, modelIDs ...string) config.Provider {
	models := make([]config.Model, len(modelIDs))
	for i, id := range modelIDs {
		models[i] = config.Model{ID: id}
	}
	return config.Provider{
		API:             config.APIChatCompletions,
		BaseURL:         baseURL,
		EnvKey:          envKey,
		DefaultModel:    defaultModel,
		Models:          models,
		MaxOutputTokens: 4096,
	}
}

func assertNoKeyLeak(t *testing.T, output string, values ...string) {
	t.Helper()
	for _, value := range values {
		if strings.Contains(output, value) {
			t.Errorf("runCheck() output contains a configured key value; only variable names may be printed:\n%s", output)
		}
	}
}

func TestRunCheckAllProvidersHealthy(t *testing.T) {
	t.Setenv(checkKeyA, checkKeyValueA)
	t.Setenv(checkKeyB, checkKeyValueB)

	chat := testutil.NewFakeChat(t, nil)
	chat.SetModels([]string{"deepseek-flash", "deepseek-v4-pro"})
	messages := testutil.NewFakeMessages(t, nil)
	messages.SetModels([]string{"claude-sonnet-5"})

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"deepseek": chatProvider(chat.URL, checkKeyA, "deepseek-flash", "deepseek-flash", "deepseek-v4-pro"),
			"anthropic": {
				API:             config.APIMessages,
				BaseURL:         messages.URL,
				EnvKey:          checkKeyB,
				DefaultModel:    "claude-sonnet-5",
				Models:          []config.Model{{ID: "claude-sonnet-5"}},
				MaxOutputTokens: 4096,
			},
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{
		"provider anthropic (messages)",
		"provider deepseek (chat-completions)",
		"  key    " + checkKeyA + "   set",
		"  api    " + chat.URL + "   OK (2 models listed)",
		"  model  deepseek-flash     OK",
		"  model  deepseek-v4-pro     OK",
		"  key    " + checkKeyB + "   set",
		"  api    " + messages.URL + "   OK (1 models listed)",
		"  model  claude-sonnet-5     OK",
		"result   PASS (2 checked, 0 skipped, 0 failed)",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	if strings.Contains(got, "FAIL") {
		t.Errorf("runCheck() output contains FAIL:\n%s", got)
	}
	assertNoKeyLeak(t, got, checkKeyValueA, checkKeyValueB)
}

func TestRunCheckAllKeysUnsetFails(t *testing.T) {
	t.Setenv(checkKeyA, "")
	t.Setenv(checkKeyB, "")

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"deepseek": chatProvider("https://api.deepseek.invalid", checkKeyA, "deepseek-flash", "deepseek-flash"),
			"openai": {
				API:          config.APIResponses,
				BaseURL:      "https://api.openai.invalid",
				EnvKey:       checkKeyB,
				DefaultModel: "gpt-test",
				Models:       []config.Model{{ID: "gpt-test"}},
			},
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 1 {
		t.Fatalf("runCheck() = %d, want 1; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{
		"  key    " + checkKeyA + "   not set, skipped",
		"  key    " + checkKeyB + "   not set, skipped",
		"result   FAIL (0 checked, 2 skipped, 0 failed) — no configured provider has its key set",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	if strings.Contains(got, "  api    ") {
		t.Errorf("runCheck() contacted a provider with no key:\n%s", got)
	}
}

func TestRunCheckSkippedProviderDoesNotFail(t *testing.T) {
	t.Setenv(checkKeyA, checkKeyValueA)
	t.Setenv(checkKeyB, "")

	chat := testutil.NewFakeChat(t, nil)
	chat.SetModels([]string{"deepseek-flash"})

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"deepseek": chatProvider(chat.URL, checkKeyA, "deepseek-flash", "deepseek-flash"),
			"anthropic": {
				API:          config.APIMessages,
				BaseURL:      "https://api.anthropic.invalid",
				EnvKey:       checkKeyB,
				DefaultModel: "claude-test",
				Models:       []config.Model{{ID: "claude-test"}},
			},
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	got := out.String()
	if want := "  key    " + checkKeyB + "   not set, skipped"; !strings.Contains(got, want) {
		t.Errorf("runCheck() output missing %q:\n%s", want, got)
	}
	if want := "result   PASS (1 checked, 1 skipped, 0 failed)"; !strings.Contains(got, want) {
		t.Errorf("runCheck() output missing %q:\n%s", want, got)
	}
	assertNoKeyLeak(t, got, checkKeyValueA)
}

func TestRunCheckModelListingFailureSkipsLive(t *testing.T) {
	t.Setenv(checkKeyA, checkKeyValueA)

	chat := testutil.NewFakeChat(t, []testutil.FakeTurn{{Text: "never reached"}})
	chat.SetModelsStatus(http.StatusUnauthorized)

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"deepseek": chatProvider(chat.URL, checkKeyA, "deepseek-flash", "deepseek-flash"),
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 1 {
		t.Fatalf("runCheck() = %d, want 1; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{
		"--live will make real, billed API calls to each configured provider.",
		"  api    " + chat.URL + "   FAIL: ",
		"401",
		"result   FAIL (1 checked, 0 skipped, 1 failed)",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	if strings.Contains(got, "  live   ") {
		t.Errorf("runCheck() attempted a live turn after a listing failure:\n%s", got)
	}
	if requests := chat.RequestCount(); requests != 0 {
		t.Errorf("provider received %d turn requests, want 0", requests)
	}
	assertNoKeyLeak(t, got, checkKeyValueA)
}

func TestRunCheckMissingModelWarnsOnly(t *testing.T) {
	t.Setenv(checkKeyA, checkKeyValueA)

	chat := testutil.NewFakeChat(t, nil)
	chat.SetModels([]string{"deepseek-flash"})

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"deepseek": chatProvider(chat.URL, checkKeyA, "deepseek-flash", "deepseek-flash", "deepseek-v4-pro"),
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{
		"  model  deepseek-flash     OK",
		"  model  deepseek-v4-pro     WARN: not in the provider's current model list",
		"result   PASS (1 checked, 0 skipped, 0 failed)",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	if strings.Contains(got, "FAIL") {
		t.Errorf("runCheck() output contains FAIL:\n%s", got)
	}
	assertNoKeyLeak(t, got, checkKeyValueA)
}

func TestRunCheckLiveRoundTripSucceeds(t *testing.T) {
	t.Setenv(checkKeyA, checkKeyValueA)

	chat := testutil.NewFakeChat(t, []testutil.FakeTurn{
		{ToolCalls: []testutil.FakeToolCall{{ID: "call_1", Name: "get_secret_number", Args: "{}"}}},
		{Text: "The secret number is 4217."},
	})
	chat.SetModels([]string{"deepseek-flash"})

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"deepseek": chatProvider(chat.URL, checkKeyA, "deepseek-flash", "deepseek-flash"),
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{
		"--live will make real, billed API calls to each configured provider.",
		"  live   deepseek-flash   OK (tool call + follow-up succeeded, ",
		"result   PASS (1 checked, 0 skipped, 0 failed)",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	if requests := chat.RequestCount(); requests != 2 {
		t.Errorf("provider received %d turn requests, want 2", requests)
	}
	assertNoKeyLeak(t, got, checkKeyValueA)
}

func TestRunCheckWithoutModelLister(t *testing.T) {
	t.Setenv(checkKeyA, checkKeyValueA)

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"custom": {
				API:          "check-no-lister",
				BaseURL:      "https://check.invalid",
				EnvKey:       checkKeyA,
				DefaultModel: "custom-model",
				Models:       []config.Model{{ID: "custom-model"}},
			},
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	got := out.String()
	if want := "  api    https://check.invalid   OK (model listing not supported)"; !strings.Contains(got, want) {
		t.Errorf("runCheck() output missing %q:\n%s", want, got)
	}
	if strings.Contains(got, "  model  ") {
		t.Errorf("runCheck() printed model lines without a model list:\n%s", got)
	}
	if want := "result   PASS (1 checked, 0 skipped, 0 failed)"; !strings.Contains(got, want) {
		t.Errorf("runCheck() output missing %q:\n%s", want, got)
	}
	assertNoKeyLeak(t, got, checkKeyValueA)
}

func TestRunCheckLiveWithoutModelLister(t *testing.T) {
	t.Setenv(checkKeyA, checkKeyValueA)

	cfg := &config.Config{
		Providers: map[string]config.Provider{
			"custom": {
				API:          "check-no-lister",
				BaseURL:      "https://check.invalid",
				EnvKey:       checkKeyA,
				DefaultModel: "custom-model",
				Models:       []config.Model{{ID: "custom-model"}},
			},
		},
	}

	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{
		"--live will make real, billed API calls to each configured provider.",
		"  live   custom-model   OK (tool call + follow-up succeeded, ",
		"result   PASS (1 checked, 0 skipped, 0 failed)",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	assertNoKeyLeak(t, got, checkKeyValueA)
}

// The adapter pools children by command. Isolate each test's pool so changes to
// fake login/model settings take effect without exposing adapter internals.
func inCodexCheckProcess(t *testing.T) bool {
	t.Helper()
	const childEnv = "SUBAGENT_CHECK_CODEX_CHILD"
	if os.Getenv(childEnv) == t.Name() {
		return true
	}
	ctx, cancel := context.WithTimeout(t.Context(), 90*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, os.Args[0], "-test.run=^"+regexp.QuoteMeta(t.Name())+"$", "-test.v")
	cmd.Env = append(os.Environ(), childEnv+"="+t.Name())
	if output, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("codex check test: %v\n%s", err, output)
	}
	return false
}

func codexCheckConfig(t *testing.T) *config.Config {
	t.Helper()
	t.Setenv("SUBAGENT_FAKE_CODEX", "1")
	t.Setenv("SUBAGENT_FAKE_CODEX_MODELS", "gpt-6-astra")
	t.Setenv("SUBAGENT_FAKE_CODEX_LOGGED_OUT", "")
	t.Setenv("SUBAGENT_FAKE_CODEX_CRASH", "")
	return &config.Config{Providers: map[string]config.Provider{
		"codex": {
			API: config.APICodexAppServer, Command: os.Args[0],
			DefaultModel: "gpt-6-astra", Models: []config.Model{{ID: "gpt-6-astra"}},
		},
	}}
}

func TestRunCheckCodexHealthy(t *testing.T) {
	if !inCodexCheckProcess(t) {
		return
	}
	cfg := codexCheckConfig(t)
	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	want := "provider codex (codex-app-server)\n" +
		"  auth   chatgpt (plus)   OK\n" +
		"  api    codex-cli 0.156.1   OK (1 models listed)\n" +
		"  model  gpt-6-astra     OK\n" +
		"result   PASS (1 checked, 0 skipped, 0 failed)\n"
	if got := out.String(); got != want {
		t.Errorf("runCheck() output =\n%s\nwant:\n%s", got, want)
	}
}

func TestRunCheckCodexLoggedOut(t *testing.T) {
	if !inCodexCheckProcess(t) {
		return
	}
	cfg := codexCheckConfig(t)
	t.Setenv("SUBAGENT_FAKE_CODEX_LOGGED_OUT", "1")
	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 1 {
		t.Fatalf("runCheck() = %d, want 1; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{
		"  auth   FAIL: not logged in; run codex login\n",
		"result   FAIL (0 checked, 0 skipped, 1 failed)",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	for _, unwanted := range []string{"  key    ", "  api    ", "  model  ", "  live   ", "no configured provider has its key set"} {
		if strings.Contains(got, unwanted) {
			t.Errorf("runCheck() printed %q after failed auth:\n%s", unwanted, got)
		}
	}
}

func TestRunCheckCodexNotInstalled(t *testing.T) {
	cfg := codexCheckConfig(t)
	p := cfg.Providers["codex"]
	p.Command = "/nonexistent/codex"
	cfg.Providers["codex"] = p
	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 1 {
		t.Fatalf("runCheck() = %d, want 1; output:\n%s", code, out.String())
	}
	got := out.String()
	for _, want := range []string{"  auth   FAIL:", "/nonexistent/codex", "result   FAIL (0 checked, 0 skipped, 1 failed)"} {
		if !strings.Contains(got, want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, got)
		}
	}
	for _, unwanted := range []string{"  key    ", "  api    ", "  model  ", "  live   ", "no configured provider has its key set"} {
		if strings.Contains(got, unwanted) {
			t.Errorf("runCheck() printed %q after failed auth:\n%s", unwanted, got)
		}
	}
}

func TestRunCheckCodexLive(t *testing.T) {
	if !inCodexCheckProcess(t) {
		return
	}
	cfg := codexCheckConfig(t)
	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	got := out.String()
	if !strings.HasPrefix(got, "--live will make real, billed API calls to each configured provider.\n") {
		t.Errorf("runCheck() did not print the billed-call warning first:\n%s", got)
	}
	if !regexp.MustCompile(`(?m)^  live   gpt-6-astra   OK \(turn succeeded, [^)]+\)$`).MatchString(got) {
		t.Errorf("runCheck() missing successful live turn with duration:\n%s", got)
	}
	if !strings.HasSuffix(got, "result   PASS (1 checked, 0 skipped, 0 failed)\n") {
		t.Errorf("runCheck() wrong result:\n%s", got)
	}
}

func TestRunCheckCodexMissingModelWarnsOnly(t *testing.T) {
	if !inCodexCheckProcess(t) {
		return
	}
	cfg := codexCheckConfig(t)
	t.Setenv("SUBAGENT_FAKE_CODEX_MODELS", "another-model")
	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	for _, want := range []string{
		"  model  gpt-6-astra     WARN: not in the provider's current model list\n",
		"result   PASS (1 checked, 0 skipped, 0 failed)\n",
	} {
		if !strings.Contains(out.String(), want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, out.String())
		}
	}
}

type checkCodexListFailure struct {
	provider.Provider
	provider.AuthChecker
}

func (*checkCodexListFailure) ListModels(context.Context) ([]string, error) {
	return nil, errors.New("model listing failed")
}

func TestRunCheckCodexModelListingFailureSkipsLive(t *testing.T) {
	if !inCodexCheckProcess(t) {
		return
	}
	cfg := codexCheckConfig(t)
	adapter, err := provider.New("codex", cfg.Providers["codex"], "")
	if err != nil {
		t.Fatal(err)
	}
	// Keep the real adapter's auth, but fail model listing in this process only.
	provider.Register(config.APICodexAppServer, func(string, config.Provider, string) (provider.Provider, error) {
		return &checkCodexListFailure{adapter, adapter.(provider.AuthChecker)}, nil
	})
	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 1 {
		t.Fatalf("runCheck() = %d, want 1; output:\n%s", code, out.String())
	}
	want := "--live will make real, billed API calls to each configured provider.\n" +
		"provider codex (codex-app-server)\n" +
		"  auth   chatgpt (plus)   OK\n" +
		"  api    FAIL: model listing failed\n" +
		"result   FAIL (1 checked, 0 skipped, 1 failed)\n"
	if got := out.String(); got != want {
		t.Errorf("runCheck() output =\n%s\nwant:\n%s", got, want)
	}
}

func TestRunCheckCodexLiveFailure(t *testing.T) {
	if !inCodexCheckProcess(t) {
		return
	}
	cfg := codexCheckConfig(t)
	t.Setenv("SUBAGENT_FAKE_CODEX_CRASH", "1")
	var out bytes.Buffer
	if code := runCheck(&out, cfg, true); code != 1 {
		t.Fatalf("runCheck() = %d, want 1; output:\n%s", code, out.String())
	}
	for _, want := range []string{
		"  live   gpt-6-astra   FAIL: codex app-server exited: fatal: boom\n",
		"result   FAIL (1 checked, 0 skipped, 1 failed)\n",
	} {
		if !strings.Contains(out.String(), want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, out.String())
		}
	}
}

func TestRunCheckCodexAndNativeCounting(t *testing.T) {
	if !inCodexCheckProcess(t) {
		return
	}
	cfg := codexCheckConfig(t)
	t.Setenv(checkKeyA, checkKeyValueA)
	t.Setenv(checkKeyB, "")
	chat := testutil.NewFakeChat(t, nil)
	chat.SetModels([]string{"native-model"})
	cfg.Providers["native"] = chatProvider(chat.URL, checkKeyA, "native-model", "native-model")
	cfg.Providers["skipped"] = chatProvider("https://skipped.invalid", checkKeyB, "unused", "unused")
	var out bytes.Buffer
	if code := runCheck(&out, cfg, false); code != 0 {
		t.Fatalf("runCheck() = %d, want 0; output:\n%s", code, out.String())
	}
	for _, want := range []string{
		"  auth   chatgpt (plus)   OK\n",
		"  key    " + checkKeyA + "   set\n",
		"  api    " + chat.URL + "   OK (1 models listed)\n",
		"  model  native-model     OK\n",
		"  key    " + checkKeyB + "   not set, skipped\n",
		"result   PASS (2 checked, 1 skipped, 0 failed)\n",
	} {
		if !strings.Contains(out.String(), want) {
			t.Errorf("runCheck() output missing %q:\n%s", want, out.String())
		}
	}
	assertNoKeyLeak(t, out.String(), checkKeyValueA)
}

type checkAgentFunc func(context.Context, provider.ThreadOptions) (provider.Thread, error)

func (f checkAgentFunc) StartThread(ctx context.Context, opts provider.ThreadOptions) (provider.Thread, error) {
	return f(ctx, opts)
}

type checkThreadStub struct {
	run    func(context.Context, string, string, provider.ThreadCallbacks) (string, error)
	closed bool
}

func (th *checkThreadStub) Run(ctx context.Context, prompt, effort string, cb provider.ThreadCallbacks) (string, error) {
	return th.run(ctx, prompt, effort, cb)
}

func (th *checkThreadStub) Close() { th.closed = true }

func TestCheckLiveThread(t *testing.T) {
	for _, tt := range []struct {
		name, reply, wantError string
		startErr, runErr       error
	}{
		{name: "success", reply: "The answer is 4217."},
		{name: "wrong answer", reply: "1234", wantError: `answer "1234" does not contain 4217`},
		{name: "run failure", runErr: errors.New("turn failed"), wantError: "turn failed"},
		{name: "start failure", startErr: errors.New("start failed"), wantError: "start failed"},
	} {
		t.Run(tt.name, func(t *testing.T) {
			var runCalled bool
			thread := &checkThreadStub{run: func(ctx context.Context, prompt, effort string, cb provider.ThreadCallbacks) (string, error) {
				runCalled = true
				if _, ok := ctx.Deadline(); !ok {
					t.Error("live turn has no deadline")
				}
				if !strings.Contains(prompt, "4217") || effort != "low" {
					t.Errorf("Run prompt = %q, effort = %q", prompt, effort)
				}
				if cb.Emit != nil || cb.Approve != nil {
					t.Error("live check should not emit events or approve operations")
				}
				return tt.reply, tt.runErr
			}}
			agent := checkAgentFunc(func(ctx context.Context, opts provider.ThreadOptions) (provider.Thread, error) {
				if _, ok := ctx.Deadline(); !ok {
					t.Error("thread start has no deadline")
				}
				want := provider.ThreadOptions{
					Model: "gpt-6-astra", Cwd: os.TempDir(), Sandbox: "read-only", ApprovalPolicy: "never",
					Ephemeral: true,
				}
				if !reflect.DeepEqual(opts, want) {
					t.Errorf("thread options = %+v, want %+v", opts, want)
				}
				if tt.startErr != nil {
					return nil, tt.startErr
				}
				return thread, nil
			})
			err := checkLiveThread(agent, "gpt-6-astra")
			if tt.wantError == "" {
				if err != nil {
					t.Errorf("checkLiveThread() = %v, want nil", err)
				}
			} else if err == nil || err.Error() != tt.wantError {
				t.Errorf("checkLiveThread() = %v, want %q", err, tt.wantError)
			}
			wantRun := tt.startErr == nil
			if runCalled != wantRun || thread.closed != wantRun {
				t.Errorf("thread ran = %v, closed = %v; want both %v", runCalled, thread.closed, wantRun)
			}
		})
	}
}
