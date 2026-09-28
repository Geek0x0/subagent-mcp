package testutil

import (
	"encoding/json"
	"fmt"
	"io"
	"sync"
	"testing"
)

// FakeCodex is a scriptable, in-memory Codex app-server. Handlers run separately
// from its reader so they can send notifications and make server-side requests.
// Scripts must release any waits of their own; Crash unblocks all protocol I/O.
type FakeCodex struct {
	t       *testing.T
	clientR *io.PipeReader
	clientW *io.PipeWriter
	reader  *io.PipeReader
	writer  *io.PipeWriter
	writeMu sync.Mutex
	once    sync.Once
	done    chan struct{}

	mu       sync.Mutex
	nextID   uint64
	handlers map[string]func(json.RawMessage) (any, error)
	received map[string][]json.RawMessage
	pending  map[string]chan json.RawMessage
}

type codexError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type codexMessage struct {
	ID     json.RawMessage `json:"id,omitempty"`
	Method string          `json:"method,omitempty"`
	Params json.RawMessage `json:"params,omitempty"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *codexError     `json:"error,omitempty"`
}

// NewFakeCodex starts an app-server with the initialize handshake accepted by
// default. The test's cleanup closes both streams.
func NewFakeCodex(t *testing.T) *FakeCodex {
	t.Helper()
	clientR, writer := io.Pipe()
	reader, clientW := io.Pipe()
	f := &FakeCodex{
		t: t, clientR: clientR, clientW: clientW, reader: reader, writer: writer,
		done:     make(chan struct{}),
		handlers: make(map[string]func(json.RawMessage) (any, error)),
		received: make(map[string][]json.RawMessage),
		pending:  make(map[string]chan json.RawMessage),
	}
	f.Handle("initialize", func(json.RawMessage) (any, error) {
		return map[string]string{"userAgent": "fake-codex"}, nil
	})
	f.Handle("initialized", func(json.RawMessage) (any, error) { return nil, nil })
	t.Cleanup(f.Crash)
	go f.read()
	return f
}

// ClientSide returns the streams to pass to codexappserver.NewClient.
func (f *FakeCodex) ClientSide() (io.Reader, io.Writer) { return f.clientR, f.clientW }

// Handle replaces the handler for a client request or notification.
func (f *FakeCodex) Handle(method string, h func(params json.RawMessage) (any, error)) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.handlers[method] = h
}

// Notify sends a server-to-client notification.
func (f *FakeCodex) Notify(method string, params any) {
	f.t.Helper()
	raw, err := json.Marshal(params)
	if err != nil {
		f.t.Errorf("fake codex notification params: %v", err)
		return
	}
	f.send(codexMessage{Method: method, Params: raw})
}

// Request sends a server-to-client request and returns the whole reply object.
// It returns nil if the fake crashes before the reply arrives.
func (f *FakeCodex) Request(method string, params any) json.RawMessage {
	f.t.Helper()
	raw, err := json.Marshal(params)
	if err != nil {
		f.t.Errorf("fake codex request params: %v", err)
		return nil
	}
	f.mu.Lock()
	f.nextID++
	// String IDs also exercise the client's preservation of server request IDs.
	id := fmt.Sprintf(`"server-%d"`, f.nextID)
	reply := make(chan json.RawMessage, 1)
	f.pending[id] = reply
	f.mu.Unlock()
	defer func() {
		f.mu.Lock()
		delete(f.pending, id)
		f.mu.Unlock()
	}()
	if !f.send(codexMessage{ID: json.RawMessage(id), Method: method, Params: raw}) {
		return nil
	}
	select {
	case response := <-reply:
		return response
	case <-f.done:
		return nil
	}
}

// Received returns independent copies of the params of client messages seen so
// far, in wire order. Notifications (including initialized) are recorded too.
func (f *FakeCodex) Received(method string) []json.RawMessage {
	f.mu.Lock()
	defer f.mu.Unlock()
	params := make([]json.RawMessage, len(f.received[method]))
	for i, raw := range f.received[method] {
		params[i] = append(json.RawMessage(nil), raw...)
	}
	return params
}

// Crash closes both streams, waking blocked readers, writers, and Requests.
func (f *FakeCodex) Crash() {
	f.once.Do(func() {
		close(f.done)
		// Closing the writer ends delivers EOF (rather than ErrClosedPipe) to
		// the readers and also releases any blocked writes on these pipes.
		_ = f.writer.Close()
		_ = f.clientW.Close()
	})
}

func (f *FakeCodex) send(msg codexMessage) bool {
	f.writeMu.Lock()
	defer f.writeMu.Unlock()
	select {
	case <-f.done:
		return false
	default:
	}
	if err := json.NewEncoder(f.writer).Encode(msg); err != nil {
		select {
		case <-f.done:
		default:
			f.t.Errorf("fake codex write: %v", err)
		}
		return false
	}
	return true
}

func (f *FakeCodex) read() {
	defer f.Crash()
	decoder := json.NewDecoder(f.reader)
	for {
		var raw json.RawMessage
		if err := decoder.Decode(&raw); err != nil {
			return
		}
		var msg codexMessage
		if err := json.Unmarshal(raw, &msg); err != nil {
			f.t.Errorf("fake codex decode: %v", err)
			return
		}
		f.mu.Lock()
		if msg.Method == "" {
			if reply := f.pending[string(msg.ID)]; reply != nil {
				delete(f.pending, string(msg.ID))
				reply <- raw
			}
			f.mu.Unlock()
			continue
		}
		f.received[msg.Method] = append(f.received[msg.Method], append(json.RawMessage(nil), msg.Params...))
		handler := f.handlers[msg.Method]
		f.mu.Unlock()
		go f.handle(msg, handler)
	}
}

func (f *FakeCodex) handle(msg codexMessage, handler func(json.RawMessage) (any, error)) {
	if handler == nil {
		if len(msg.ID) != 0 {
			f.send(codexMessage{ID: msg.ID, Error: &codexError{Code: -32601, Message: "method not found"}})
		}
		return
	}
	result, err := handler(msg.Params)
	if len(msg.ID) == 0 {
		return
	}
	if err != nil {
		f.send(codexMessage{ID: msg.ID, Error: &codexError{Code: -32000, Message: err.Error()}})
		return
	}
	raw, err := json.Marshal(result)
	if err != nil {
		f.t.Errorf("fake codex result: %v", err)
		f.Crash()
		return
	}
	f.send(codexMessage{ID: msg.ID, Result: raw})
}
