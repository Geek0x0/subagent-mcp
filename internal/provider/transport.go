package provider

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// Transport kind identifiers for SharedTransport. A kind is part of the
// registry key because each adapter family fixes its own transport
// properties; adapters and their tests must use these constants so they always
// meet in the same registry entry.
const (
	KindMessages        = "messages"
	KindResponses       = "responses"
	KindChatCompletions = "chat-completions"
)

// sharedTransports maps one (kind, base URL) pair to the transport every
// adapter instance for that pair reuses. The registry never grows past the
// distinct base URLs in the server's configuration — a small, fixed set, one
// entry per configured provider endpoint — so growth is bounded by
// construction; creation is lazy and every entry lives for the process.
var (
	sharedTransportsMu sync.Mutex
	sharedTransports   = map[string]*http.Transport{}
)

// SharedTransport returns the connection pool shared by every adapter built
// for the same (kind, baseURL): sessions against one endpoint reuse TLS/TCP
// connections instead of building a fresh SDK transport — and paying a fresh
// handshake — for every session. Only the pool is shared. Each adapter still
// builds its own SDK client carrying its own credential, because the API key
// differs per session and per provider.
//
// The transport is a clone of http.DefaultTransport, so proxy-from-environment
// and the standard TLS defaults carry over unchanged (no InsecureSkipVerify);
// responseHeaderTimeout bounds the wait for response headers (0 leaves that
// unbounded, which is what the responses and chat-completions adapters had).
// The transport is built once, under the registry lock, on first use: every
// caller for a kind must pass that kind's timeout, and callers must not mutate
// the returned transport afterwards.
func SharedTransport(kind, baseURL string, responseHeaderTimeout time.Duration) *http.Transport {
	key := kind + "\x00" + baseURL

	sharedTransportsMu.Lock()
	defer sharedTransportsMu.Unlock()
	if transport, ok := sharedTransports[key]; ok {
		return transport
	}

	var transport *http.Transport
	if base, ok := http.DefaultTransport.(*http.Transport); ok {
		transport = base.Clone()
	} else {
		// http.DefaultTransport was replaced with a custom RoundTripper:
		// fall back to a transport that still honors environment proxies
		// rather than sharing a pool this registry cannot describe.
		transport = &http.Transport{Proxy: http.ProxyFromEnvironment}
	}
	transport.ResponseHeaderTimeout = responseHeaderTimeout
	sharedTransports[key] = transport
	return transport
}

// Bounds on the discard of an unread response body on Close: at most drainLimit
// bytes, and at most drainTimeout of waiting, so a server that sends the final
// sentinel but keeps the response open cannot stall the caller.
const (
	drainLimit   = 64 << 10
	drainTimeout = 250 * time.Millisecond
)

// PooledClient returns an HTTP client over SharedTransport(kind, baseURL, ...)
// whose response bodies are drained on Close. The SSE readers stop at the final
// sentinel without reading the body to EOF; Go's transport only returns a
// connection to the pool once the body has hit EOF, so without the drain every
// streamed turn would close its connection and the pool would never be reused.
// The drain is bounded by size and time (drainLimit, drainTimeout); a cancelled
// request makes the read fail at once.
func PooledClient(kind, baseURL string, responseHeaderTimeout time.Duration) *http.Client {
	return PooledClientWithBody(kind, baseURL, responseHeaderTimeout, nil)
}

// PooledClientWithBody is PooledClient that also merges extraBody into the JSON
// object of every POST request body, for fields the SDK has no place for (a
// gateway's upstream-routing preferences, say). The pool stays shared; the extra
// fields belong to this client only.
func PooledClientWithBody(kind, baseURL string, responseHeaderTimeout time.Duration, extraBody map[string]any) *http.Client {
	return &http.Client{Transport: drainingTransport{
		base: SharedTransport(kind, baseURL, responseHeaderTimeout), kind: kind, extra: extraBody,
	}}
}

type drainingTransport struct {
	base  http.RoundTripper
	kind  string
	extra map[string]any
}

func (d drainingTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	if len(d.extra) > 0 {
		merged, err := mergeJSONBody(req, d.extra)
		if err != nil {
			return nil, err
		}
		req = merged
	}
	resp, err := d.base.RoundTrip(req)
	if err == nil && resp.Body != nil {
		if recorder, ok := req.Context().Value(upstreamKey{}).(*upstreamRecorder); ok {
			resp.Body = &upstreamBody{ReadCloser: resp.Body, recorder: recorder}
		}
		if dir := os.Getenv(DebugStreamEnv); dir != "" {
			resp.Body = recordStream(dir, d.kind, req, resp)
		}
		resp.Body = drainOnClose{resp.Body}
	}
	return resp, err
}

// mergeJSONBody returns a copy of req whose JSON object body also carries the
// extra fields (they win over fields of the same name). A request that is not a
// POST with a JSON object body is returned unchanged.
func mergeJSONBody(req *http.Request, extra map[string]any) (*http.Request, error) {
	if req.Method != http.MethodPost || req.Body == nil || !strings.Contains(req.Header.Get("Content-Type"), "json") {
		return req, nil
	}
	original, err := io.ReadAll(req.Body)
	_ = req.Body.Close()
	if err != nil {
		return nil, err
	}
	body := original
	var fields map[string]json.RawMessage
	if json.Unmarshal(original, &fields) == nil && fields != nil {
		for name, value := range extra {
			encoded, err := json.Marshal(value)
			if err != nil {
				return nil, fmt.Errorf("extra_body field %q: %w", name, err)
			}
			fields[name] = encoded
		}
		if merged, err := json.Marshal(fields); err == nil {
			body = merged
		}
	}
	clone := req.Clone(req.Context())
	clone.Body = io.NopCloser(bytes.NewReader(body))
	clone.ContentLength = int64(len(body))
	clone.GetBody = func() (io.ReadCloser, error) { return io.NopCloser(bytes.NewReader(body)), nil }
	return clone, nil
}

type upstreamKey struct{}

// upstreamRecorder receives the name of the upstream channel a gateway reports
// in the first stream chunk (the "provider" field of OpenRouter-style
// responses).
type upstreamRecorder struct {
	mu   sync.Mutex
	name string
	done bool
	seen int
}

// WithUpstreamRecorder returns a context under which the HTTP responses of
// PooledClient clients are scanned for the serving upstream, and a function that
// returns it (empty when the API names none).
func WithUpstreamRecorder(ctx context.Context) (context.Context, func() string) {
	recorder := &upstreamRecorder{}
	return context.WithValue(ctx, upstreamKey{}, recorder), func() string {
		recorder.mu.Lock()
		defer recorder.mu.Unlock()
		return recorder.name
	}
}

var upstreamField = regexp.MustCompile(`"provider"\s*:\s*"([^"\\]{1,64})"`)

// upstreamScanLimit bounds how much of a response is scanned for the field; it
// always sits in the first chunk, if present.
const upstreamScanLimit = 16 << 10

type upstreamBody struct {
	io.ReadCloser
	recorder *upstreamRecorder
	window   []byte
}

func (b *upstreamBody) Read(p []byte) (int, error) {
	n, err := b.ReadCloser.Read(p)
	b.recorder.mu.Lock()
	defer b.recorder.mu.Unlock()
	if n > 0 && !b.recorder.done {
		b.window = append(b.window, p[:n]...)
		if match := upstreamField.FindSubmatch(b.window); match != nil {
			b.recorder.name, b.recorder.done = string(match[1]), true
		} else if len(b.window) >= upstreamScanLimit {
			b.recorder.done = true
		}
	}
	return n, err
}

// DebugStreamEnv names a directory; when set, the raw response of every model API
// request (status, a few headers, and the body exactly as received) is written
// there, one file per request, for diagnosing what a server really sent. Request
// headers and bodies are never recorded, so no key or prompt reaches the file.
const DebugStreamEnv = "SUBAGENT_MCP_DEBUG_STREAM"

var (
	debugStreamSeq  atomic.Uint64
	debugStreamOnce sync.Once
)

// recordStream tees resp.Body into <dir>/<time>-<kind>-<n>.sse. A failure to
// record is logged once and the response is passed through untouched.
func recordStream(dir, kind string, req *http.Request, resp *http.Response) io.ReadCloser {
	fail := func(err error) io.ReadCloser {
		debugStreamOnce.Do(func() { log.Printf("%s: cannot record the response stream: %v", DebugStreamEnv, err) })
		return resp.Body
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fail(err)
	}
	name := fmt.Sprintf("%s-%s-%d.sse", time.Now().UTC().Format("20060102T150405.000"), kind, debugStreamSeq.Add(1))
	file, err := os.OpenFile(filepath.Join(dir, name), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return fail(err)
	}
	// The URL is recorded without query or credentials.
	fmt.Fprintf(file, "# %s %s://%s%s\n# status %d\n", req.Method, req.URL.Scheme, req.URL.Host, req.URL.Path, resp.StatusCode)
	for _, header := range []string{"Content-Type", "X-Request-Id", "Request-Id"} {
		if value := resp.Header.Get(header); value != "" {
			fmt.Fprintf(file, "# %s: %s\n", header, value)
		}
	}
	fmt.Fprint(file, "\n")
	return &recordedBody{ReadCloser: resp.Body, file: file}
}

type recordedBody struct {
	io.ReadCloser
	file *os.File
}

func (b *recordedBody) Read(p []byte) (int, error) {
	n, err := b.ReadCloser.Read(p)
	if n > 0 {
		_, _ = b.file.Write(p[:n])
	}
	return n, err
}

func (b *recordedBody) Close() error {
	err := b.ReadCloser.Close()
	_ = b.file.Close()
	return err
}

type drainOnClose struct{ io.ReadCloser }

func (b drainOnClose) Close() error {
	drained := make(chan struct{})
	go func() {
		defer close(drained)
		_, _ = io.CopyN(io.Discard, b.ReadCloser, drainLimit)
	}()
	timer := time.NewTimer(drainTimeout)
	defer timer.Stop()
	select {
	case <-drained:
	case <-timer.C:
	}
	// Closing also unblocks a drain that is still reading.
	return b.ReadCloser.Close()
}
