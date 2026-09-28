package config

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const validTOML = `

[providers.deepseek]
api = "chat-completions"
base_url = "https://api.deepseek.com"
env_key = "DS_TEST_KEY"
default_model = "deepseek-flash"
models = [
  { id = "deepseek-flash", description = "fast" },
  { id = "deepseek-v4-pro" },
]
effort_map = { medium = "high", xhigh = "max" }

[providers.anthropic]
api = "messages"
env_key = "ANT_TEST_KEY"
default_model = "claude-sonnet-5"
models = [{ id = "claude-sonnet-5" }]
`

const codexTOML = `
[providers.codex]
api = "codex-app-server"
default_model = "gpt-6-astra"
models = [{ id = "gpt-6-astra" }]
`

func writeConfig(t *testing.T, content string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return path
}

func TestLoadValid(t *testing.T) {
	cfg, err := Load(writeConfig(t, validTOML))
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	p, err := cfg.Provider("deepseek")
	if err != nil {
		t.Fatalf("Provider(deepseek) error = %v", err)
	}
	if p.API != APIChatCompletions || p.BaseURL != "https://api.deepseek.com" {
		t.Fatalf("Provider(deepseek) = %#v", p)
	}
	if !reflect.DeepEqual(p.ModelIDs(), []string{"deepseek-flash", "deepseek-v4-pro"}) {
		t.Fatalf("ModelIDs() = %v", p.ModelIDs())
	}
	if p.MapEffort("xhigh") != "max" || p.MapEffort("low") != "low" {
		t.Fatalf("MapEffort wrong")
	}
	anthropic := cfg.Providers["anthropic"]
	if anthropic.BaseURL != "https://api.anthropic.com" || anthropic.MaxOutputTokens != 64000 {
		t.Fatalf("defaults not applied: %#v", anthropic)
	}
	if !reflect.DeepEqual(cfg.EnvKeys(), []string{"ANT_TEST_KEY", "DS_TEST_KEY"}) {
		t.Fatalf("EnvKeys() = %v", cfg.EnvKeys())
	}
	if !reflect.DeepEqual(cfg.ProviderNames(), []string{"anthropic", "deepseek"}) {
		t.Fatalf("ProviderNames() = %v", cfg.ProviderNames())
	}
	if cfg.Path == "" {
		t.Fatalf("Path not recorded")
	}
}

func TestLoadCodexAppServerProvider(t *testing.T) {
	cfg, err := Load(writeConfig(t, codexTOML))
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	p := cfg.Providers["codex"]
	if p.API != APICodexAppServer || p.BaseURL != "" || p.Command != "" {
		t.Fatalf("Provider(codex) = %#v", p)
	}
	if key, err := cfg.APIKeyFor("codex"); err != nil || key != "" {
		t.Fatalf("APIKeyFor(codex) = %q, %v, want empty key and nil error", key, err)
	}
	if keys := cfg.EnvKeys(); len(keys) != 0 {
		t.Fatalf("EnvKeys() = %q, want no keys", keys)
	}
}

func TestLoadCodexAppServerCommand(t *testing.T) {
	cfg, err := Load(writeConfig(t, codexTOML+`command = "/opt/codex/bin/codex"`))
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if got := cfg.Providers["codex"].Command; got != "/opt/codex/bin/codex" {
		t.Fatalf("Command = %q, want /opt/codex/bin/codex", got)
	}
}

func TestEnvKeys(t *testing.T) {
	content := validTOML + codexTOML + strings.Replace(codexTOML, "providers.codex", "providers.codex2", 1)
	content += strings.ReplaceAll(validTOML, "providers.", "providers.other-")
	cfg, err := Load(writeConfig(t, content))
	if err != nil {
		t.Fatal(err)
	}
	if keys := cfg.EnvKeys(); !reflect.DeepEqual(keys, []string{"ANT_TEST_KEY", "DS_TEST_KEY"}) {
		t.Fatalf("EnvKeys() = %q, want sorted, deduplicated, non-empty keys", keys)
	}
}

func TestLoadRelativePathStoredAbsolute(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "config.toml"), []byte(validTOML), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Chdir(dir)

	// t.Chdir sets PWD to the path it was given and filepath.Abs prefers $PWD,
	// so cfg.Path may keep a symlinked form of the cwd (for example /var rather
	// than /private/var on macOS). Resolve both sides before comparing; the
	// IsAbs check is what guards the fix itself.
	check := func(t *testing.T, base string) {
		t.Helper()
		cfg, err := Load("config.toml")
		if err != nil {
			t.Fatalf("Load() error = %v", err)
		}
		if !filepath.IsAbs(cfg.Path) {
			t.Fatalf("cfg.Path = %q, want an absolute path", cfg.Path)
		}
		got, err := filepath.EvalSymlinks(cfg.Path)
		if err != nil {
			t.Fatalf("EvalSymlinks(%q): %v", cfg.Path, err)
		}
		want, err := filepath.EvalSymlinks(filepath.Join(base, "config.toml"))
		if err != nil {
			t.Fatalf("EvalSymlinks(%q): %v", filepath.Join(base, "config.toml"), err)
		}
		if got != want {
			t.Fatalf("cfg.Path resolves to %q, want %q", got, want)
		}
	}

	check(t, dir)

	t.Run("via symlinked cwd", func(t *testing.T) {
		link := filepath.Join(t.TempDir(), "link")
		if err := os.Symlink(dir, link); err != nil {
			t.Fatal(err)
		}
		t.Chdir(link)
		check(t, link)
	})
}

func TestAPIKeyFor(t *testing.T) {
	cfg, err := Load(writeConfig(t, validTOML))
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("DS_TEST_KEY", "")
	if _, err := cfg.APIKeyFor("deepseek"); err == nil || !strings.Contains(err.Error(), "DS_TEST_KEY") {
		t.Fatalf("APIKeyFor() error = %v, want mention of DS_TEST_KEY", err)
	}
	t.Setenv("DS_TEST_KEY", "sk-1")
	if key, err := cfg.APIKeyFor("deepseek"); err != nil || key != "sk-1" {
		t.Fatalf("APIKeyFor() = %q, %v", key, err)
	}
	if _, err := cfg.APIKeyFor("nope"); err == nil || !strings.Contains(err.Error(), "nope") || !strings.Contains(err.Error(), "deepseek") {
		t.Fatalf("APIKeyFor(nope) error = %v, want it to name the unknown provider and list the configured ones", err)
	}
}

func TestProviderLookup(t *testing.T) {
	cfg, err := Load(writeConfig(t, validTOML))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := cfg.Provider("deepseek"); err != nil {
		t.Fatalf("Provider(deepseek) error = %v", err)
	}
	_, err = cfg.Provider("nope")
	if err == nil || !strings.Contains(err.Error(), "nope") || !strings.Contains(err.Error(), "anthropic") || !strings.Contains(err.Error(), "deepseek") {
		t.Fatalf("Provider(nope) error = %v, want it to name the unknown provider and list anthropic and deepseek", err)
	}
}

func TestLoadErrors(t *testing.T) {
	tests := []struct {
		name    string
		content string
		want    string
	}{
		{"unknown top key", "typo = 1\n", "typo"},
		{"unknown provider key", strings.Replace(validTOML, `env_key = "ANT_TEST_KEY"`, "env_key = \"ANT_TEST_KEY\"\nmodel = \"x\"", 1), "model"},
		{"bad api", strings.Replace(validTOML, `api = "messages"`, `api = "grpc"`, 1), "providers.anthropic.api"},
		{"missing env_key", strings.Replace(validTOML, `env_key = "ANT_TEST_KEY"`, "", 1), "providers.anthropic.env_key"},
		{"codex env_key", codexTOML + `env_key = "X"`, "providers.codex.env_key is not used by codex-app-server: authentication goes through codex login"},
		{"codex base_url", codexTOML + `base_url = "https://x"`, "providers.codex.base_url is not used by codex-app-server"},
		{"native command", validTOML + `command = "codex"`, "providers.anthropic.command is only valid for codex-app-server"},
		{"chat-completions command", strings.Replace(validTOML, `api = "chat-completions"`, "api = \"chat-completions\"\ncommand = \"codex\"", 1), "providers.deepseek.command is only valid for codex-app-server"},
		{"responses command", strings.Replace(validTOML, `api = "chat-completions"`, "api = \"responses\"\ncommand = \"codex\"", 1), "providers.deepseek.command is only valid for codex-app-server"},
		{"codex missing default_model", strings.Replace(codexTOML, `default_model = "gpt-6-astra"`, "", 1), "providers.codex.default_model"},
		{"default not in models", strings.Replace(validTOML, `default_model = "claude-sonnet-5"`, `default_model = "claude-opus-5"`, 1), "providers.anthropic.default_model"},
		{"empty models", strings.Replace(validTOML, `models = [{ id = "claude-sonnet-5" }]`, "models = []", 1), "providers.anthropic.models"},
		{"duplicate model", strings.Replace(validTOML, `{ id = "deepseek-v4-pro" }`, `{ id = "deepseek-flash" }`, 1), "providers.deepseek.models"},
		{"bad effort key", strings.Replace(validTOML, `medium = "high"`, `turbo = "high"`, 1), "providers.deepseek.effort_map"},
		{"bad effort value", strings.Replace(validTOML, `medium = "high"`, `medium = "turbo"`, 1), "providers.deepseek.effort_map"},
		{"negative max tokens", strings.Replace(validTOML, `api = "messages"`, "api = \"messages\"\nmax_output_tokens = -1", 1), "providers.anthropic.max_output_tokens"},
		{"invalid provider name", strings.Replace(validTOML, "[providers.deepseek]", `[providers."bad name!"]`, 1), "not a valid provider name"},
		{"no providers", "providers = {}\n", "at least one provider"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			_, err := Load(writeConfig(t, test.content))
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("Load() error = %v, want it to mention %q", err, test.want)
			}
		})
	}
}

func TestLoadRejectsLegacyActiveProviderWithUpgradeHint(t *testing.T) {
	const legacy = `
active_provider = "deepseek"

[providers.deepseek]
api = "chat-completions"
env_key = "DS_TEST_KEY"
default_model = "deepseek-flash"
models = [{ id = "deepseek-flash" }]
`
	_, err := Load(writeConfig(t, legacy))
	if err == nil {
		t.Fatal("Load() error = nil, want a rejection naming active_provider")
	}
	if !strings.Contains(err.Error(), "active_provider was removed") || !strings.Contains(err.Error(), "0.8.0") {
		t.Fatalf("Load() error = %v, want it to explain active_provider was removed in 0.8.0", err)
	}
}

func TestLoadMissingFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "missing.toml")
	_, err := Load(path)
	if err == nil || !strings.Contains(err.Error(), path) || !strings.Contains(err.Error(), "config.example.toml") {
		t.Fatalf("Load() error = %v, want path and example hint", err)
	}
}

func TestDefaultPath(t *testing.T) {
	t.Setenv("SUBAGENT_MCP_CONFIG", "/custom/config.toml")
	if got, _ := DefaultPath(); got != "/custom/config.toml" {
		t.Fatalf("DefaultPath() = %q", got)
	}
	t.Setenv("SUBAGENT_MCP_CONFIG", "")
	t.Setenv("HOME", "/home/test")
	if got, _ := DefaultPath(); got != "/home/test/.config/subagent-mcp/config.toml" {
		t.Fatalf("DefaultPath() = %q", got)
	}
}

func TestValidEffort(t *testing.T) {
	for _, value := range EffortValues {
		if !ValidEffort(value) {
			t.Errorf("ValidEffort(%q) = false", value)
		}
	}
	if ValidEffort("turbo") {
		t.Errorf("ValidEffort(turbo) = true")
	}
}

func TestExampleConfigLoads(t *testing.T) {
	cfg, err := Load("../../config.example.toml")
	if err != nil {
		t.Fatalf("config.example.toml does not load: %v", err)
	}
	for _, name := range []string{"deepseek", "openai", "anthropic"} {
		if _, ok := cfg.Providers[name]; !ok {
			t.Errorf("example lacks provider %q", name)
		}
	}
}
