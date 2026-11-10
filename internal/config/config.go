// Package config loads the subagent-mcp TOML configuration.
package config

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
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

// EffortValues lists the reasoning effort values accepted from callers. The last
// is what Codex calls maximum reasoning with automatic task delegation; the model
// API families have no equivalent, so a provider that needs a different value
// maps it with effort_map.
var EffortValues = []string{"none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"}

var providerNamePattern = regexp.MustCompile(`^[A-Za-z0-9_.-]+$`)

// Model describes one model advertised by a provider.
type Model struct {
	ID          string `toml:"id"`
	Description string `toml:"description"`
}

// MaxNudgesLimit is the largest accepted max_nudges.
const MaxNudgesLimit = 10

// DefaultMaxOutputTokens is the max_output_tokens a messages provider gets when
// the config does not set one. The Anthropic Messages API requires max_tokens,
// so that api has no "let the server decide" option. The other model APIs do,
// which is why they get no default: unset means the request carries no output
// limit and the provider's own default applies.
const DefaultMaxOutputTokens = 64000

// Provider holds the configuration of a single API provider.
type Provider struct {
	API     string `toml:"api"`
	BaseURL string `toml:"base_url"`
	EnvKey  string `toml:"env_key"`
	// Key is the API key itself, as an alternative to env_key. A config that
	// holds one must not be readable by other users (see Load).
	Key             string            `toml:"key"`
	Command         string            `toml:"command"`
	DefaultModel    string            `toml:"default_model"`
	Models          []Model           `toml:"models"`
	EffortMap       map[string]string `toml:"effort_map"`
	MaxOutputTokens int               `toml:"max_output_tokens"`
	// MaxNudges is the default number of "continue" prompts a run may send when
	// the model replies without a tool call; 0 (the default) disables it.
	MaxNudges int `toml:"max_nudges"`
	// ExtraBody holds fields merged into the JSON body of every chat-completions
	// request, for what the SDK request type has no place for, such as an
	// OpenRouter-style gateway's upstream routing (provider.only / provider.ignore).
	ExtraBody map[string]any `toml:"extra_body"`
}

// reservedBodyFields are request fields the server owns; extra_body may not set
// them. max_tokens belongs here because max_output_tokens now sends it: before
// that, extra_body was the only way to cap a chat-completions call, so a config
// may still carry it, and letting an extra field win would silently ignore the
// configured limit. max_completion_tokens stays free on purpose — OpenAI's
// reasoning models reject max_tokens and take that field instead.
var reservedBodyFields = map[string]bool{
	"model": true, "messages": true, "stream": true, "stream_options": true, "tools": true, "tool_choice": true,
	"max_tokens": true,
}

// Auto configures the optional TypeSafe Jev decision model used by the start
// tool's "auto" model value.
type Auto struct {
	BaseURL        string `toml:"base_url"`
	URL            string `toml:"url"`
	EnvKey         string `toml:"env_key"`
	Key            string `toml:"key"`
	Model          string `toml:"model"`
	TimeoutSeconds int    `toml:"timeout_seconds"`
}

const (
	DefaultAutoBaseURL        = "https://api.typesafe.ai"
	DefaultAutoTimeoutSeconds = 20
	MaxAutoTimeoutSeconds     = 600
)

// Config is the root of the configuration file.
type Config struct {
	Path      string              `toml:"-"`
	Providers map[string]Provider `toml:"providers"`
	Auto      *Auto               `toml:"auto"`
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
	file, err := os.Open(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, fmt.Errorf("config file %s not found; copy config.example.toml from the subagent-mcp repository to that path or set SUBAGENT_MCP_CONFIG", path)
	}
	if err != nil {
		return nil, fmt.Errorf("read config file %s: %w", path, err)
	}
	defer file.Close()
	// Stat the opened file, so the mode checked below is the mode of what was read.
	info, err := file.Stat()
	if err != nil {
		return nil, fmt.Errorf("read config file %s: %w", path, err)
	}
	data, err := io.ReadAll(file)
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
			// Name the keys only: the decoder's own report quotes the source
			// lines, which would put a mistyped key's value into the error.
			unknown := make([]string, 0, len(strict.Errors))
			for _, fieldErr := range strict.Errors {
				unknown = append(unknown, strings.Join(fieldErr.Key(), "."))
			}
			return nil, fmt.Errorf("config file %s: unknown keys: %s", path, strings.Join(unknown, ", "))
		}
		var decodeErr *toml.DecodeError
		if errors.As(err, &decodeErr) {
			// Same reason: report where and what, never the offending line.
			row, column := decodeErr.Position()
			return nil, fmt.Errorf("config file %s: line %d, column %d: %s", path, row, column, decodeErr.Error())
		}
		return nil, fmt.Errorf("config file %s: %w", path, err)
	}
	if cfg.holdsKey() && runtime.GOOS != "windows" && info.Mode().Perm()&0o077 != 0 {
		return nil, fmt.Errorf("config file %s holds an API key but its mode is %04o, so other users can read it; run: chmod 600 %s", path, info.Mode().Perm(), path)
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
		if provider.MaxOutputTokens == 0 && provider.API == APIMessages {
			// Only the Messages API has no way to say "the server decides".
			provider.MaxOutputTokens = DefaultMaxOutputTokens
		}
		cfg.Providers[name] = provider
	}
	if cfg.Auto != nil {
		if cfg.Auto.BaseURL == "" && cfg.Auto.URL == "" {
			cfg.Auto.BaseURL = DefaultAutoBaseURL
		}
		if cfg.Auto.TimeoutSeconds == 0 {
			cfg.Auto.TimeoutSeconds = DefaultAutoTimeoutSeconds
		}
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
	if c.Auto != nil {
		if c.Auto.EnvKey == "" && c.Auto.Key == "" {
			return errors.New("auto.env_key or auto.key is required")
		}
		if c.Auto.EnvKey != "" && c.Auto.Key != "" {
			return errors.New("auto.env_key and auto.key cannot both be set")
		}
		if c.Auto.Model == "" {
			return errors.New("auto.model is required")
		}
		if c.Auto.URL != "" && c.Auto.BaseURL != "" {
			return errors.New("auto.url and auto.base_url cannot both be set")
		}
		endpoint, field := c.Auto.BaseURL, "auto.base_url"
		if c.Auto.URL != "" {
			endpoint, field = c.Auto.URL, "auto.url"
		}
		u, err := url.Parse(endpoint)
		if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" || u.RawQuery != "" || u.ForceQuery || u.Fragment != "" {
			return fmt.Errorf("%s %q must be an absolute http or https URL", field, endpoint)
		}
		if c.Auto.TimeoutSeconds < 0 || c.Auto.TimeoutSeconds > MaxAutoTimeoutSeconds {
			return fmt.Errorf("auto.timeout_seconds must be 0 (default) or between 1 and %d", MaxAutoTimeoutSeconds)
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
		if p.Key != "" {
			return fmt.Errorf("%s.key is not used by codex-app-server: authentication goes through codex login", prefix)
		}
		if p.BaseURL != "" {
			return fmt.Errorf("%s.base_url is not used by codex-app-server", prefix)
		}
		if p.MaxOutputTokens != 0 {
			return fmt.Errorf("%s.max_output_tokens is not used by codex-app-server: the app-server applies its own limits", prefix)
		}
	} else {
		if p.Command != "" {
			return fmt.Errorf("%s.command is only valid for codex-app-server", prefix)
		}
		if p.EnvKey == "" && p.Key == "" {
			return fmt.Errorf("%s.env_key or %s.key is required", prefix, prefix)
		}
		if p.EnvKey != "" && p.Key != "" {
			return fmt.Errorf("%s.env_key and %s.key cannot both be set", prefix, prefix)
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
		if model.ID == "auto" {
			return fmt.Errorf("%s.models[%d].id %q is reserved for the start tool's auto model selection; rename it", prefix, i, model.ID)
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
	// The Messages API requires max_tokens, so a provider using it must carry a
	// positive value. Load() supplies the default; a config built in code does
	// not, and an unset field there would send an unusable zero.
	if p.API == APIMessages && p.MaxOutputTokens == 0 {
		return fmt.Errorf("%s.max_output_tokens must be set for %s (the API requires max_tokens); %d is the usual value", prefix, APIMessages, DefaultMaxOutputTokens)
	}
	if len(p.ExtraBody) > 0 {
		if p.API != APIChatCompletions {
			return fmt.Errorf("%s.extra_body is only supported for api = %q", prefix, APIChatCompletions)
		}
		for name := range p.ExtraBody {
			if name == "max_tokens" {
				return fmt.Errorf("%s.extra_body.max_tokens is set by the server from max_output_tokens; set %s.max_output_tokens instead, or use extra_body.max_completion_tokens for a model that requires it", prefix, prefix)
			}
			if reservedBodyFields[name] {
				return fmt.Errorf("%s.extra_body.%s is set by the server and cannot be overridden", prefix, name)
			}
		}
		// max_completion_tokens is the escape hatch for a model that rejects
		// max_tokens, so it may be set — but not alongside max_output_tokens,
		// which would send both fields and have the API reject the pair.
		if _, present := p.ExtraBody["max_completion_tokens"]; present && p.MaxOutputTokens != 0 {
			return fmt.Errorf("%s sets both max_output_tokens and extra_body.max_completion_tokens, which would send both limits; keep max_completion_tokens and remove max_output_tokens, or the other way round", prefix)
		}
	}
	if p.MaxNudges < 0 || p.MaxNudges > MaxNudgesLimit {
		return fmt.Errorf("%s.max_nudges must be between 0 and %d", prefix, MaxNudgesLimit)
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

// holdsKey reports whether any provider or auto carries its key in the file.
func (c *Config) holdsKey() bool {
	for _, p := range c.Providers {
		if p.Key != "" {
			return true
		}
	}
	return c.Auto != nil && c.Auto.Key != ""
}

// APIKeyFor returns the named provider's API key: its key field, or else the
// value of its env_key variable.
// Codex app-server providers use codex login and return an empty key.
func (c *Config) APIKeyFor(name string) (string, error) {
	p, err := c.Provider(name)
	if err != nil {
		return "", err
	}
	if p.API == APICodexAppServer {
		return "", nil
	}
	if p.Key != "" {
		return p.Key, nil
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

// EnvKeys returns every non-empty provider and auto env_key, sorted and
// deduplicated.
func (c *Config) EnvKeys() []string {
	set := map[string]bool{}
	for _, p := range c.Providers {
		if p.EnvKey != "" {
			set[p.EnvKey] = true
		}
	}
	if c.Auto != nil && c.Auto.EnvKey != "" {
		set[c.Auto.EnvKey] = true
	}
	keys := make([]string, 0, len(set))
	for key := range set {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}

// AutoAPIKey returns the Jev key from auto.key, or else from auto.env_key.
func (c *Config) AutoAPIKey() (string, error) {
	if c.Auto == nil {
		return "", errors.New("auto model selection is not configured")
	}
	if c.Auto.Key != "" {
		return c.Auto.Key, nil
	}
	key := os.Getenv(c.Auto.EnvKey)
	if key == "" {
		return "", fmt.Errorf("environment variable %s (auto.env_key) must be set to the decision model API key", c.Auto.EnvKey)
	}
	return key, nil
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
