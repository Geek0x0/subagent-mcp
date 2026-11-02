// Package decision asks TypeSafe Jev's System One endpoint to choose the
// provider and model best suited to a task.
package decision

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/Geek0x0/subagent-mcp/internal/config"
)

var (
	httpClientOnce   sync.Once
	sharedHTTPClient *http.Client
)

const (
	maxCandidates       = 255
	maxResponseBytes    = 1 << 20
	maxErrorDetailRunes = 512

	modelQuestion = "model"
	choiceType    = "choice"

	modelInstructions = "Select the configured provider and model best suited to complete the task in state. Weigh the task's required reasoning depth, coding and tool needs, and the speed/cost implied by each candidate's description. Choose exactly one candidate."
)

// Candidate is one provider/model pair offered to the decision model.
type Candidate struct {
	Provider    string
	Model       string
	API         string
	Description string
}

// Selection is the provider/model pair the decision model chose.
type Selection struct {
	Provider      string
	Model         string
	JudgeModel    string
	Confidence    float64
	Probabilities map[string]float64
	InputTokens   int
	OutputTokens  int
}

// Client calls the TypeSafe Jev System One endpoint. Its fields are unexported
// so callers cannot mutate the HTTP behavior after construction.
type Client struct {
	baseURL    string
	apiKey     string
	model      string
	timeout    time.Duration
	httpClient *http.Client
}

type systemOneRequest struct {
	Model     string                    `json:"model"`
	State     map[string]string         `json:"state"`
	Questions map[string]choiceQuestion `json:"questions"`
}

type choiceQuestion struct {
	Type         string            `json:"type"`
	Instructions string            `json:"instructions"`
	Criteria     map[string]string `json:"criteria"`
}

type systemOneResponse struct {
	Model   string                  `json:"model"`
	Answers map[string]choiceAnswer `json:"answers"`
	Usage   systemOneUsage          `json:"usage"`
}

type choiceAnswer struct {
	Type          string             `json:"type"`
	Choice        string             `json:"choice"`
	Confidence    float64            `json:"confidence"`
	Probabilities map[string]float64 `json:"probabilities"`
}

type systemOneUsage struct {
	InputTokens  int `json:"input_tokens"`
	OutputTokens int `json:"output_tokens"`
}

// New returns a client for cfg. An empty BaseURL or TimeoutSeconds takes the
// package default. Clients share a clone of http.DefaultTransport, so
// proxy-from-environment and the standard TLS defaults carry over.
func New(cfg config.Auto, apiKey string) *Client {
	if cfg.BaseURL == "" {
		cfg.BaseURL = config.DefaultAutoBaseURL
	}
	if cfg.TimeoutSeconds == 0 {
		cfg.TimeoutSeconds = config.DefaultAutoTimeoutSeconds
	}

	httpClientOnce.Do(func() {
		var transport http.RoundTripper = http.DefaultTransport
		if base, ok := http.DefaultTransport.(*http.Transport); ok {
			transport = base.Clone()
		}
		sharedHTTPClient = &http.Client{Transport: transport}
	})

	return &Client{
		baseURL:    cfg.BaseURL,
		apiKey:     apiKey,
		model:      cfg.Model,
		timeout:    time.Duration(cfg.TimeoutSeconds) * time.Second,
		httpClient: sharedHTTPClient,
	}
}

// Choose asks Jev to choose one of candidates for task in cwd. It fails rather
// than guesses when the answer is missing, not a choice, or names an unoffered
// candidate.
func (c *Client) Choose(ctx context.Context, task, cwd string, candidates []Candidate) (Selection, error) {
	if len(candidates) == 0 {
		return Selection{}, fmt.Errorf("systemone: no candidates to choose from")
	}
	if len(candidates) > maxCandidates {
		return Selection{}, fmt.Errorf("systemone: %d candidates exceed the maximum of %d", len(candidates), maxCandidates)
	}

	criteria := make(map[string]string, len(candidates))
	choices := make(map[string]Candidate, len(candidates))
	for _, candidate := range candidates {
		key := candidateKey(candidate)
		criteria[key] = candidateDescription(candidate)
		choices[key] = candidate
	}

	body, err := json.Marshal(systemOneRequest{
		Model: c.model,
		State: map[string]string{"task": task, "cwd": cwd},
		Questions: map[string]choiceQuestion{
			modelQuestion: {
				Type:         choiceType,
				Instructions: modelInstructions,
				Criteria:     criteria,
			},
		},
	})
	if err != nil {
		return Selection{}, fmt.Errorf("systemone: encode request: %w", err)
	}

	if c.timeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, c.timeout)
		defer cancel()
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, strings.TrimRight(c.baseURL, "/")+"/v1/systemone", bytes.NewReader(body))
	if err != nil {
		return Selection{}, fmt.Errorf("systemone: build request: %w", err)
	}
	req.Header.Set("Authorization", "Bearer "+c.apiKey)
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return Selection{}, fmt.Errorf("systemone: request: %w", err)
	}
	defer resp.Body.Close()

	responseBody, err := io.ReadAll(io.LimitReader(resp.Body, maxResponseBytes+1))
	if err != nil {
		return Selection{}, fmt.Errorf("systemone: read response: %w", err)
	}
	if len(responseBody) > maxResponseBytes {
		return Selection{}, fmt.Errorf("systemone: response body too large (limit %d bytes)", maxResponseBytes)
	}

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		return Selection{}, fmt.Errorf("systemone: status %d: %s", resp.StatusCode, errorDetail(responseBody, resp.StatusCode))
	}

	var answer systemOneResponse
	if err := json.Unmarshal(responseBody, &answer); err != nil {
		return Selection{}, fmt.Errorf("systemone: decode response: %w", err)
	}

	choice, ok := answer.Answers[modelQuestion]
	if !ok {
		return Selection{}, fmt.Errorf("systemone: model %q returned no answer for question %q", answer.Model, modelQuestion)
	}
	if choice.Type != choiceType {
		return Selection{}, fmt.Errorf("systemone: model %q answer type %q is not %q", answer.Model, choice.Type, choiceType)
	}
	candidate, ok := choices[choice.Choice]
	if !ok {
		return Selection{}, fmt.Errorf("systemone: model %q chose unknown choice %q", answer.Model, choice.Choice)
	}

	return Selection{
		Provider:      candidate.Provider,
		Model:         candidate.Model,
		JudgeModel:    answer.Model,
		Confidence:    choice.Confidence,
		Probabilities: choice.Probabilities,
		InputTokens:   answer.Usage.InputTokens,
		OutputTokens:  answer.Usage.OutputTokens,
	}, nil
}

func candidateKey(candidate Candidate) string {
	return candidate.Provider + "/" + candidate.Model
}

func candidateDescription(candidate Candidate) string {
	description := fmt.Sprintf("provider %s (api %s); model %s", candidate.Provider, candidate.API, candidate.Model)
	if candidate.Description != "" {
		description += "; " + candidate.Description
	}
	return description
}

// errorDetail renders the response's detail field and never the request. It
// accepts either a string or a JSON value and bounds the result.
func errorDetail(body []byte, status int) string {
	var payload struct {
		Detail json.RawMessage `json:"detail"`
	}
	if err := json.Unmarshal(body, &payload); err != nil || len(payload.Detail) == 0 {
		return truncate(http.StatusText(status), maxErrorDetailRunes)
	}

	var text string
	if err := json.Unmarshal(payload.Detail, &text); err == nil {
		return truncate(text, maxErrorDetailRunes)
	}

	var compact bytes.Buffer
	if err := json.Compact(&compact, payload.Detail); err == nil {
		return truncate(compact.String(), maxErrorDetailRunes)
	}
	return truncate(string(payload.Detail), maxErrorDetailRunes)
}

func truncate(text string, limit int) string {
	runes := []rune(text)
	if len(runes) <= limit {
		return text
	}
	return string(runes[:limit])
}
