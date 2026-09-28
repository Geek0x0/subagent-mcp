// Package config loads the subagent-mcp TOML configuration.
package config

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"

	"github.com/pelletier/go-toml/v2"
)

// API protocol identifiers accepted by the api field of a provider.
const (
	APIChatCompletions = "chat-completions"
	APIResponses       = "responses"
	APIMessages        = "messages"
	APICodexAppServer  = "codex-app-server"
)

// EffortValues lists the reasoning effort values accepted from callers.
var EffortValues = []string{"none", "minimal", "low", "medium", "high", "xhigh", "max"}

var providerNamePattern = regexp.MustCompile(`^[A-Za-z0-9_.-]+$`)

// Model describes one model advertised by a provider.
type Model struct {
	ID          string `toml:"id"`
	Description string `toml:"description"`
}

// Provider holds the configuration of a single API provider.
type Provider struct {
	API             string            `toml:"api"`
	BaseURL         string            `toml:"base_url"`
	EnvKey          string            `toml:"env_key"`
	Command         string            `toml:"command"`
	DefaultModel    string            `toml:"default_model"`
	Models          []Model           `toml:"models"`
	EffortMap       map[string]string `toml:"effort_map"`
	MaxOutputTokens int               `toml:"max_output_tokens"`
}

// Config is the root of the configuration file.
type Config struct {
	Path      string              `toml:"-"`
	Providers map[string]Provider `toml:"providers"`
}

// DefaultPath returns $SUBAGENT_MCP_CONFIG or ~/.config/subagent-mcp/config.toml.
func DefaultPath() (string, error) {
	if path := os.Getenv("SUBAGENT_MCP_CONFIG"); path != "" {
		return path, nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("resolve home directory for the default config path: %w", err)
	}
	return filepath.Join(home, ".config", "subagent-mcp", "config.toml"), nil
}

// Load reads, strictly decodes, defaults and validates the config at path.
func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, fmt.Errorf("config file %s not found; copy config.example.toml from the subagent-mcp repository to that path or set SUBAGENT_MCP_CONFIG", path)
	}
	if err != nil {
		return nil, fmt.Errorf("read config file %s: %w", path, err)
	}
	var cfg Config
	decoder := toml.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&cfg); err != nil {
		var strict *toml.StrictMissingError
		if errors.As(err, &strict) {
			for _, fieldErr := range strict.Errors {
				key := fieldErr.Key()
				if len(key) == 1 && key[0] == "active_provider" {
					return nil, fmt.Errorf("config file %s: active_provider was removed in subagent-mcp 0.8.0; delete that line and select a provider per call with the start tool's provider argument instead", path)
				}
			}
			return nil, fmt.Errorf("config file %s: unknown keys: %s", path, strict.String())
		}
		return nil, fmt.Errorf("config file %s: %w", path, err)
	}
	absolute, err := filepath.Abs(path)
	if err != nil {
		return nil, fmt.Errorf("resolve config file path %s: %w", path, err)
	}
	cfg.Path = absolute
	for name, provider := range cfg.Providers {
		if provider.BaseURL == "" {
			switch provider.API {
			case APIMessages:
				provider.BaseURL = "https://api.anthropic.com"
			case APIChatCompletions, APIResponses:
				provider.BaseURL = "https://api.openai.com/v1"
			}
		}
		if provider.MaxOutputTokens == 0 {
			provider.MaxOutputTokens = 64000
		}
		cfg.Providers[name] = provider
	}
	if err := cfg.Validate(); err != nil {
		return nil, fmt.Errorf("config file %s: %w", path, err)
	}
	return &cfg, nil
}

// Validate requires at least one provider and checks every provider's fields.
func (c *Config) Validate() error {
	if len(c.Providers) == 0 {
		return errors.New("providers must define at least one provider")
	}
	names := make([]string, 0, len(c.Providers))
	for name := range c.Providers {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		if !providerNamePattern.MatchString(name) {
			return fmt.Errorf("providers %q is not a valid provider name; use only letters, digits, '.', '_', or '-'", name)
		}
		if err := c.Providers[name].validate("providers." + name); err != nil {
			return err
		}
	}
	return nil
}

func (p Provider) validate(prefix string) error {
	switch p.API {
	case APIChatCompletions, APIResponses, APIMessages, APICodexAppServer:
	default:
		return fmt.Errorf("%s.api must be one of %s, %s, %s, %s; got %q", prefix, APIChatCompletions, APIResponses, APIMessages, APICodexAppServer, p.API)
	}
	if p.API == APICodexAppServer {
		if p.EnvKey != "" {
			return fmt.Errorf("%s.env_key is not used by codex-app-server: authentication goes through codex login", prefix)
		}
		if p.BaseURL != "" {
			return fmt.Errorf("%s.base_url is not used by codex-app-server", prefix)
		}
	} else {
		if p.Command != "" {
			return fmt.Errorf("%s.command is only valid for codex-app-server", prefix)
		}
		if p.EnvKey == "" {
			return fmt.Errorf("%s.env_key is required", prefix)
		}
	}
	if len(p.Models) == 0 {
		return fmt.Errorf("%s.models must list at least one model", prefix)
	}
	seen := map[string]bool{}
	for i, model := range p.Models {
		if model.ID == "" {
			return fmt.Errorf("%s.models[%d].id is required", prefix, i)
		}
		if seen[model.ID] {
			return fmt.Errorf("%s.models has duplicate id %q", prefix, model.ID)
		}
		seen[model.ID] = true
	}
	if !seen[p.DefaultModel] {
		return fmt.Errorf("%s.default_model %q must be one of %s.models", prefix, p.DefaultModel, prefix)
	}
	for from, to := range p.EffortMap {
		if !ValidEffort(from) || !ValidEffort(to) {
			return fmt.Errorf("%s.effort_map entry %q = %q: keys and values must be one of %v", prefix, from, to, EffortValues)
		}
	}
	if p.MaxOutputTokens < 0 {
		return fmt.Errorf("%s.max_output_tokens must be positive", prefix)
	}
	return nil
}

// Provider returns the named provider's configuration, or an error listing
// every configured provider name.
func (c *Config) Provider(name string) (Provider, error) {
	p, ok := c.Providers[name]
	if !ok {
		return Provider{}, fmt.Errorf("unknown provider %q; configured providers: %s", name, strings.Join(c.ProviderNames(), ", "))
	}
	return p, nil
}

// APIKeyFor returns the named provider's API key from its env_key variable.
// Codex app-server providers use codex login and return an empty key.
func (c *Config) APIKeyFor(name string) (string, error) {
	p, err := c.Provider(name)
	if err != nil {
		return "", err
	}
	if p.API == APICodexAppServer {
		return "", nil
	}
	key := os.Getenv(p.EnvKey)
	if key == "" {
		return "", fmt.Errorf("environment variable %s (providers.%s.env_key) must be set to the API key", p.EnvKey, name)
	}
	return key, nil
}

// ProviderNames returns every configured provider name, sorted.
func (c *Config) ProviderNames() []string {
	names := make([]string, 0, len(c.Providers))
	for name := range c.Providers {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

// EnvKeys returns every non-empty provider env_key, sorted and deduplicated.
func (c *Config) EnvKeys() []string {
	set := map[string]bool{}
	for _, p := range c.Providers {
		if p.EnvKey != "" {
			set[p.EnvKey] = true
		}
	}
	keys := make([]string, 0, len(set))
	for key := range set {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}

// HasModel reports whether id is one of the provider's models.
func (p Provider) HasModel(id string) bool {
	for _, model := range p.Models {
		if model.ID == id {
			return true
		}
	}
	return false
}

// ModelIDs returns the provider's model ids in config order.
func (p Provider) ModelIDs() []string {
	ids := make([]string, len(p.Models))
	for i, model := range p.Models {
		ids[i] = model.ID
	}
	return ids
}

// MapEffort returns effort_map[value] if present, else value.
func (p Provider) MapEffort(value string) string {
	if mapped, ok := p.EffortMap[value]; ok {
		return mapped
	}
	return value
}

// ValidEffort reports whether value is one of EffortValues.
func ValidEffort(value string) bool {
	for _, known := range EffortValues {
		if value == known {
			return true
		}
	}
	return false
}
