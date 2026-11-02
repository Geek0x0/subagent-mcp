package decision

import "strings"

var defaultDescriptions = map[string]string{
	"claude-opus-5":            "Anthropic flagship; strongest reasoning, long agentic and multi-file coding; slower, most expensive.",
	"claude-opus-5-5":          "Anthropic flagship; strongest reasoning, long agentic and multi-file coding; slower, most expensive.",
	"claude-sonnet-5":          "Anthropic balanced tier; strong coding and tool use; moderate speed and cost.",
	"claude-sonnet-5-5":        "Anthropic balanced tier; strong coding and tool use; moderate speed and cost.",
	"claude-haiku-5-5":         "Anthropic small model; simple edits, search, and summaries; fast and cheap.",
	"gpt-6-astra":              "OpenAI top tier; hardest coding and long autonomous runs; slowest, most expensive.",
	"gpt-6.1-sol":              "OpenAI everyday default; strong coding, research, and analysis; mid speed and cost.",
	"gpt-6.1-terra":            "OpenAI cost tier; much faster and about half Sol's cost; somewhat weaker on hard coding.",
	"gpt-6-luna":               "OpenAI lightweight model; quick questions, summaries, and simple edits; very cheap, not for complex code.",
	"deepseek-v4.1-flash":      "DeepSeek agentic coding model; good for tool-heavy and long-context tasks; fast and very cheap.",
	"deepseek-v4-flash":        "DeepSeek V4 fast tier; cheap, weaker than V4.1 Flash on agentic coding.",
	"gemini-3.6-flash":         "Google Flash; solid coding and tool use without Pro cost; fast and cheap.",
	"glm-5.1":                  "Z.ai GLM previous generation; coding-capable general model; mid cost.",
	"glm-5.3":                  "Z.ai latest GLM; tuned for coding and strong at it; low cost.",
	"glm-5.3-flash":            "Z.ai GLM fast tier; suited to simple tasks; fast and cheap.",
	"kimi-k3":                  "Moonshot flagship thinking model; strongest Kimi for complex reasoning and agentic work; slower.",
	"kimi-k2.7-code":           "Moonshot coding-focused model; good for coding agents; moderate cost.",
	"kimi-k2.7-code-highspeed": "Moonshot K2.7 Code faster variant; good for coding agents; quicker responses.",
	"kimi-k2.6":                "Moonshot previous flagship with open weights; solid coding; cheaper than K3.",
	"mimo-v2.6-pro":            "Xiaomi MiMo flagship; 1M context and decent coding; low cost.",
	"mimo-v2.6-flash":          "Xiaomi MiMo fast tier with open weights; 1M context, weaker coding; very cheap.",
}

// Description returns the configured description or a default for a known model.
func Description(model, configured string) string {
	if configured != "" {
		return configured
	}
	if description, ok := defaultDescriptions[model]; ok {
		return description
	}
	if i := strings.LastIndexByte(model, '/'); i >= 0 {
		return defaultDescriptions[model[i+1:]]
	}
	return ""
}
