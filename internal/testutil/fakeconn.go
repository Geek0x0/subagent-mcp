package testutil

import (
	"net"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
)

// newConnCountingServer starts an httptest server serving mux and counts every
// TCP connection it accepts (http.ConnState StateNew). The count is what
// proves whether two sessions against one endpoint really reuse one
// connection or each pay a fresh handshake.
func newConnCountingServer(mux *http.ServeMux, conns *atomic.Int64) *httptest.Server {
	server := httptest.NewUnstartedServer(mux)
	server.Config.ConnState = func(_ net.Conn, state http.ConnState) {
		if state == http.StateNew {
			conns.Add(1)
		}
	}
	server.Start()

	return server
}
