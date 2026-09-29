package provider

import (
	"io"
	"net/http"
	"sync"
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

// drainLimit bounds how much of an unread response body is discarded on Close.
const drainLimit = 64 << 10

// PooledClient returns an HTTP client over SharedTransport(kind, baseURL, ...)
// whose response bodies are drained on Close. The SSE readers stop at the final
// sentinel without reading the body to EOF; Go's transport only returns a
// connection to the pool once the body has hit EOF, so without the drain every
// streamed turn would close its connection and the pool would never be reused.
// The drain is bounded (drainLimit) and returns at once when the request was
// cancelled, because the read then fails.
func PooledClient(kind, baseURL string, responseHeaderTimeout time.Duration) *http.Client {
	return &http.Client{Transport: drainingTransport{SharedTransport(kind, baseURL, responseHeaderTimeout)}}
}

type drainingTransport struct{ base http.RoundTripper }

func (d drainingTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	resp, err := d.base.RoundTrip(req)
	if err == nil && resp.Body != nil {
		resp.Body = drainOnClose{resp.Body}
	}
	return resp, err
}

type drainOnClose struct{ io.ReadCloser }

func (b drainOnClose) Close() error {
	_, _ = io.CopyN(io.Discard, b.ReadCloser, drainLimit)
	return b.ReadCloser.Close()
}
