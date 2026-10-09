package provider

import (
	"net/http"
	"testing"
	"time"
)

// TestPooledClientBoundsResponseHeaders pins that every adapter family gets a
// bounded wait for response headers, all the way through the client the adapters
// actually hand to their SDK. The messages adapter had a bound and the
// chat-completions and responses adapters passed zero — which net/http reads as
// "no timeout" — so a gateway that accepted the connection and then never
// answered hung the turn, and the thread lock behind it, until the caller
// cancelled. A zero here means that regression is back.
//
// The assertion is on the client's own transport rather than on the registry, so
// it fails if a client ever stops building its transport through SharedTransport.
func TestPooledClientBoundsResponseHeaders(t *testing.T) {
	for _, kind := range []string{KindChatCompletions, KindResponses, KindMessages} {
		t.Run(kind, func(t *testing.T) {
			// A base URL unique to this subtest, so the registry cannot hand back
			// a transport some earlier test built.
			client := PooledClient(kind, "https://timeout-test.invalid/"+kind)

			draining, ok := client.Transport.(drainingTransport)
			if !ok {
				t.Fatalf("client transport = %T, want the draining transport the pool builds", client.Transport)
			}
			transport, ok := draining.base.(*http.Transport)
			if !ok {
				t.Fatalf("pooled transport = %T, want *http.Transport", draining.base)
			}
			if transport.ResponseHeaderTimeout == 0 {
				t.Fatal("ResponseHeaderTimeout is 0, which net/http reads as no timeout: a server that never answers would hang the turn")
			}
			if transport.ResponseHeaderTimeout != DefaultResponseHeaderTimeout {
				t.Errorf("ResponseHeaderTimeout = %v, want the shared default %v", transport.ResponseHeaderTimeout, DefaultResponseHeaderTimeout)
			}
		})
	}
}

// TestDefaultResponseHeaderTimeoutIsUsable pins the value itself: the bound has
// to be long enough for a slow gateway to answer and short enough to matter.
func TestDefaultResponseHeaderTimeoutIsUsable(t *testing.T) {
	if DefaultResponseHeaderTimeout < time.Minute {
		t.Errorf("DefaultResponseHeaderTimeout = %v, want at least a minute so a slow gateway is not cut off", DefaultResponseHeaderTimeout)
	}
	if DefaultResponseHeaderTimeout > 30*time.Minute {
		t.Errorf("DefaultResponseHeaderTimeout = %v, want well under half an hour so a wedged endpoint is not waited on", DefaultResponseHeaderTimeout)
	}
}

// TestSharedTransportKeepsOneTransportPerEndpoint pins the pooling contract the
// registry exists for: the same (kind, base URL) always returns the same
// transport, and a different kind or URL does not.
func TestSharedTransportKeepsOneTransportPerEndpoint(t *testing.T) {
	first := SharedTransport(KindResponses, "https://pool-test.invalid/a")
	if again := SharedTransport(KindResponses, "https://pool-test.invalid/a"); again != first {
		t.Error("the same (kind, base URL) returned two transports, so the pool would not be shared")
	}
	if other := SharedTransport(KindResponses, "https://pool-test.invalid/b"); other == first {
		t.Error("a different base URL returned the same transport")
	}
	if otherKind := SharedTransport(KindMessages, "https://pool-test.invalid/a"); otherKind == first {
		t.Error("a different kind returned the same transport")
	}
}
