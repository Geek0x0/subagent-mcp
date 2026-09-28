// Package codexappserver implements the Codex app-server protocol.
package codexappserver

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strconv"
	"sync"
)

// RPCError is an error returned by the remote endpoint.
type RPCError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

func (e *RPCError) Error() string { return e.Message }

// Message is a Codex request, notification, or response. Codex omits the
// JSON-RPC version field. IDs remain raw JSON to preserve both strings and numbers.
type Message struct {
	ID     json.RawMessage `json:"id,omitempty"`
	Method string          `json:"method,omitempty"`
	Params json.RawMessage `json:"params,omitempty"`
	Result json.RawMessage `json:"result,omitempty"`
	Error  *RPCError       `json:"error,omitempty"`
}

type callReply struct {
	message Message
	err     error
}

// Client multiplexes calls and per-thread events over a single connection.
// The caller owns the streams and must close the transport to stop the reader.
type Client struct {
	writeMu       sync.Mutex
	encoder       *json.Encoder
	mu            sync.Mutex
	nextID        uint64
	pending       map[string]chan callReply
	subscriptions map[string]*Subscription
	err           error
	done          chan struct{}
}

// NewClient starts a single reader goroutine. Subscriptions need no goroutines.
func NewClient(r io.Reader, w io.Writer) *Client {
	c := &Client{
		encoder:       json.NewEncoder(w),
		pending:       make(map[string]chan callReply),
		subscriptions: make(map[string]*Subscription),
		done:          make(chan struct{}),
	}
	go c.read(r)
	return c
}

// Call sends a request and decodes its result, unless result is nil.
func (c *Client) Call(ctx context.Context, method string, params, result any) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	raw, err := marshalParams(params)
	if err != nil {
		return fmt.Errorf("encode %s params: %w", method, err)
	}

	// Allocate IDs under the write lock so they also increase in wire order.
	c.writeMu.Lock()
	if err := ctx.Err(); err != nil {
		c.writeMu.Unlock()
		return err
	}
	c.mu.Lock()
	if c.err != nil {
		err := c.err
		c.mu.Unlock()
		c.writeMu.Unlock()
		return fmt.Errorf("call %s: %w", method, err)
	}
	c.nextID++
	id := strconv.FormatUint(c.nextID, 10)
	reply := make(chan callReply, 1)
	c.pending[id] = reply
	c.mu.Unlock()
	err = c.encoder.Encode(Message{ID: json.RawMessage(id), Method: method, Params: raw})
	c.writeMu.Unlock()
	defer func() {
		c.mu.Lock()
		delete(c.pending, id)
		c.mu.Unlock()
	}()
	if err != nil {
		return fmt.Errorf("write %s: %w", method, err)
	}

	select {
	case response := <-reply:
		if response.err != nil {
			return fmt.Errorf("call %s: %w", method, response.err)
		}
		if response.message.Error != nil {
			return response.message.Error
		}
		if result != nil {
			if err := json.Unmarshal(response.message.Result, result); err != nil {
				return fmt.Errorf("decode %s result: %w", method, err)
			}
		}
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// Notify sends a notification, which has no request ID or response.
func (c *Client) Notify(method string, params any) error {
	raw, err := marshalParams(params)
	if err != nil {
		return err
	}
	return c.write(Message{Method: method, Params: raw})
}

// Subscribe returns the existing queue for threadID, or registers a new one.
func (c *Client) Subscribe(threadID string) *Subscription {
	c.mu.Lock()
	defer c.mu.Unlock()
	if sub := c.subscriptions[threadID]; sub != nil {
		return sub
	}
	sub := &Subscription{changed: make(chan struct{})}
	if c.err != nil {
		sub.close(c.err)
	}
	c.subscriptions[threadID] = sub
	return sub
}

// Unsubscribe removes a thread's queue and wakes its waiters. Requests still
// queued for that thread are rejected rather than leaving the server waiting.
func (c *Client) Unsubscribe(threadID string) {
	c.mu.Lock()
	sub := c.subscriptions[threadID]
	delete(c.subscriptions, threadID)
	var queued []Message
	if sub != nil {
		queued = sub.close(errors.New("codex app-server subscription closed"))
	}
	c.mu.Unlock()
	for _, msg := range queued {
		if len(msg.ID) != 0 {
			_ = c.RespondError(msg.ID, -32601, "not supported by subagent-mcp")
		}
	}
}

// Respond answers a server request, preserving the request's ID type.
func (c *Client) Respond(id json.RawMessage, result any) error {
	raw, err := json.Marshal(result)
	if err != nil {
		return err
	}
	return c.write(Message{ID: id, Result: raw})
}

// RespondError answers a server request with a protocol error.
func (c *Client) RespondError(id json.RawMessage, code int, message string) error {
	return c.write(Message{ID: id, Error: &RPCError{Code: code, Message: message}})
}

// Done closes when the reader stops and all pending calls and queues are failed.
func (c *Client) Done() <-chan struct{} { return c.done }

// Err reports why the reader stopped, or nil while it is running.
func (c *Client) Err() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.err
}

func marshalParams(params any) (json.RawMessage, error) {
	if params == nil {
		return nil, nil
	}
	return json.Marshal(params)
}

func (c *Client) write(msg Message) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	if err := c.Err(); err != nil {
		return err
	}
	// Encoder emits exactly one JSON object and newline per serialized write.
	return c.encoder.Encode(msg)
}

func (c *Client) read(r io.Reader) {
	decoder := json.NewDecoder(r)
	for {
		var msg Message
		if err := decoder.Decode(&msg); err != nil {
			c.stop(err)
			return
		}
		if msg.Method == "" {
			c.mu.Lock()
			if reply := c.pending[string(msg.ID)]; reply != nil {
				delete(c.pending, string(msg.ID))
				reply <- callReply{message: msg} // Buffered, even for a canceled caller.
			}
			c.mu.Unlock()
			continue
		}

		var params struct {
			ThreadID string `json:"threadId"`
		}
		var routed bool
		if json.Unmarshal(msg.Params, &params) == nil && params.ThreadID != "" {
			c.mu.Lock()
			if sub := c.subscriptions[params.ThreadID]; sub != nil {
				sub.enqueue(msg)
				routed = true
			}
			c.mu.Unlock()
		}
		if !routed && len(msg.ID) != 0 {
			if err := c.RespondError(msg.ID, -32601, "not supported by subagent-mcp"); err != nil {
				c.stop(err)
				return
			}
		}
	}
}

func (c *Client) stop(err error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.err = err
	for id, reply := range c.pending {
		reply <- callReply{err: err}
		delete(c.pending, id)
	}
	for _, sub := range c.subscriptions {
		sub.close(err)
	}
	close(c.done)
}

// Subscription is an unbounded, in-order queue. A slow consumer never holds up
// the shared reader. Closing a queue discards events and wakes every waiter.
type Subscription struct {
	mu      sync.Mutex
	queue   []Message
	changed chan struct{}
	err     error
}

func (s *Subscription) enqueue(msg Message) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.queue = append(s.queue, msg)
	// Broadcast only on the empty-to-nonempty transition. No consumer can be
	// asleep with a nonempty queue, except one already woken by this signal.
	if len(s.queue) == 1 {
		close(s.changed)
		s.changed = make(chan struct{})
	}
}

func (s *Subscription) close(err error) []Message {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.err != nil {
		return nil
	}
	s.err = err
	queued := s.queue
	s.queue = nil
	close(s.changed)
	return queued
}

// Next waits for the next event, context cancellation, or the queue's closure.
func (s *Subscription) Next(ctx context.Context) (Message, error) {
	for {
		if err := ctx.Err(); err != nil {
			return Message{}, err
		}
		s.mu.Lock()
		if s.err != nil {
			err := s.err
			s.mu.Unlock()
			return Message{}, err
		}
		if len(s.queue) != 0 {
			msg := s.queue[0]
			s.queue[0] = Message{} // Do not retain consumed payloads.
			s.queue = s.queue[1:]
			if len(s.queue) == 0 {
				s.queue = nil
			}
			s.mu.Unlock()
			return msg, nil
		}
		changed := s.changed
		s.mu.Unlock()
		select {
		case <-changed:
		case <-ctx.Done():
			return Message{}, ctx.Err()
		}
	}
}
