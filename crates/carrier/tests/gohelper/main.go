// Command gohelper is the Go side of osewa-carrier's interop tests: a
// coder/websocket client and server configured as osewa's (server.go
// serveWS, client.go dialWS).
//
//	gohelper client <addr> <mode> <test> [data]
//	gohelper server <mode>
//	gohelper bench <data>
//
// Client modes: osewa (context takeover, never compressing, as osewa's
// client), ctx (context takeover, compressing from 128 bytes), noctx (no
// context takeover, compressing from 512 bytes), none. Server modes: ctx
// (as osewa's server), none (--no-compress), nosub (no subprotocol).
//
// The client prints "ok" and exits 0 when its test passed; the server
// prints "addr <addr>", then a line per event.
package main

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"math"
	"math/rand"
	"net"
	"net/http"
	"os"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"
)

const (
	subprotocol = "osewa.v0"
	maxFrame    = 1 << 20
)

func main() {
	var err error
	switch os.Args[1] {
	case "client":
		data := ""
		if len(os.Args) > 5 {
			data = os.Args[5]
		}
		err = client(os.Args[2], os.Args[3], os.Args[4], data)
	case "server":
		err = server(os.Args[2])
	case "bench":
		err = bench(os.Args[2])
	default:
		err = fmt.Errorf("unknown command %q", os.Args[1])
	}
	if err != nil {
		fmt.Println("FAIL", err)
		os.Exit(1)
	}
}

type countConn struct {
	net.Conn
	read *atomic.Int64
}

func (c countConn) Read(p []byte) (int, error) {
	n, err := c.Conn.Read(p)
	c.read.Add(int64(n))
	return n, err
}

func dial(ctx context.Context, addr, mode string, subprotocols []string, read *atomic.Int64) (*websocket.Conn, error) {
	opts := &websocket.DialOptions{Subprotocols: subprotocols}
	switch mode {
	case "osewa":
		opts.CompressionMode = websocket.CompressionContextTakeover
		opts.CompressionThreshold = math.MaxInt
	case "ctx":
		opts.CompressionMode = websocket.CompressionContextTakeover
	case "noctx":
		opts.CompressionMode = websocket.CompressionNoContextTakeover
	case "none":
	default:
		return nil, fmt.Errorf("unknown mode %q", mode)
	}
	opts.HTTPClient = &http.Client{Transport: &http.Transport{
		DialContext: func(ctx context.Context, network, a string) (net.Conn, error) {
			c, err := (&net.Dialer{}).DialContext(ctx, network, a)
			if err != nil {
				return nil, err
			}
			return countConn{c, read}, nil
		},
	}}
	ws, _, err := websocket.Dial(ctx, "ws://"+addr+"/osewa", opts)
	if err != nil {
		return nil, err
	}
	ws.SetReadLimit(maxFrame)
	return ws, nil
}

// messages are the sizes and kinds the echo test sends.
func messages() [][]byte {
	var ms [][]byte
	for _, n := range []int{0, 1, 127, 128, 129, 511, 512, 4096, 65535, 65536, 300000, maxFrame} {
		ms = append(ms, text(n, n))
	}
	noise := make([]byte, 200000)
	rand.New(rand.NewSource(7)).Read(noise)
	ms = append(ms, noise)
	for i := range 500 {
		ms = append(ms, text(200+i%50, i))
	}
	return ms
}

func text(n, seed int) []byte {
	line := fmt.Sprintf("drwxr-xr-x  5 osewa staff  160 Oct 30 12:%02d \x1b[1;34msrc%d\x1b[0m\r\n", seed%60, seed)
	return bytes.Repeat([]byte(line), n/len(line)+1)[:n]
}

// frames cuts terminal output into OUTPUT frames of varying size, as
// osewa-carrier's tests do.
func frames(data []byte) [][]byte {
	sizes := []int{64, 300, 1500, 4096, 16384}
	var fs [][]byte
	for i := 0; len(data) > 0; i++ {
		n := min(sizes[i%len(sizes)], len(data))
		f := append([]byte{0xA5, 1, 0, 0, 0}, data[:n]...)
		fs = append(fs, f)
		data = data[n:]
	}
	return fs
}

func roundTrip(ctx context.Context, ws *websocket.Conn, m []byte) error {
	if err := ws.Write(ctx, websocket.MessageBinary, m); err != nil {
		return err
	}
	typ, b, err := ws.Read(ctx)
	if err != nil {
		return err
	}
	if typ != websocket.MessageBinary || !bytes.Equal(b, m) {
		return fmt.Errorf("echo of %d bytes came back as %d bytes (type %v)", len(m), len(b), typ)
	}
	return nil
}

func wantClose(ctx context.Context, ws *websocket.Conn, code websocket.StatusCode, reason string) error {
	_, _, err := ws.Read(ctx)
	var ce websocket.CloseError
	if !errors.As(err, &ce) {
		return fmt.Errorf("read: want close %d, got %v", code, err)
	}
	if ce.Code != code || (reason != "" && ce.Reason != reason) {
		return fmt.Errorf("read: want close %d %q, got %d %q", code, reason, ce.Code, ce.Reason)
	}
	return nil
}

func client(addr, mode, test, data string) error {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	subs := []string{subprotocol}
	if test == "nosub" {
		subs = nil
	}
	var read atomic.Int64
	ws, err := dial(ctx, addr, mode, subs, &read)
	if err != nil {
		return err
	}
	defer ws.CloseNow()
	switch test {
	case "echo":
		for _, m := range messages() {
			if err := roundTrip(ctx, ws, m); err != nil {
				return err
			}
		}
		if data != "" {
			b, err := os.ReadFile(data)
			if err != nil {
				return err
			}
			before, payload := read.Load(), 0
			for _, f := range frames(b) {
				if err := roundTrip(ctx, ws, f); err != nil {
					return err
				}
				payload += len(f)
			}
			fmt.Printf("ratio %d %d\n", payload, read.Load()-before)
		}
		// Our pong is read by a reader: this one, which then waits for
		// the answer to "ping-me" (the server pinging us).
		pinged := make(chan error, 1)
		go func() {
			_, b, err := ws.Read(ctx)
			if err == nil && string(b) != "pinged" {
				err = fmt.Errorf("ping-me: got %q", b)
			}
			pinged <- err
		}()
		if err := ws.Ping(ctx); err != nil {
			return fmt.Errorf("ping: %w", err)
		}
		if err := ws.Write(ctx, websocket.MessageBinary, []byte("ping-me")); err != nil {
			return err
		}
		if err := <-pinged; err != nil {
			return err
		}
		if err := ws.Close(websocket.StatusNormalClosure, "done"); err != nil {
			return fmt.Errorf("close: %w", err)
		}
	case "text":
		if err := ws.Write(ctx, websocket.MessageText, []byte("hello")); err != nil {
			return err
		}
		if err := wantClose(ctx, ws, websocket.StatusUnsupportedData, "binary frames only"); err != nil {
			return err
		}
	case "big":
		go ws.Write(ctx, websocket.MessageBinary, make([]byte, maxFrame+1))
		if err := wantClose(ctx, ws, websocket.StatusMessageTooBig, "read limited at 1048577 bytes"); err != nil {
			return err
		}
	case "nosub":
		if err := wantClose(ctx, ws, websocket.StatusPolicyViolation, "subprotocol osewa.v0 required"); err != nil {
			return err
		}
	case "close-me":
		if err := ws.Write(ctx, websocket.MessageBinary, []byte("close-me")); err != nil {
			return err
		}
		if err := wantClose(ctx, ws, 4000, "rust bye"); err != nil {
			return err
		}
	default:
		return fmt.Errorf("unknown test %q", test)
	}
	fmt.Println("ok")
	return nil
}

func server(mode string) error {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return err
	}
	fmt.Println("addr", ln.Addr())
	var ids atomic.Int64
	mux := http.NewServeMux()
	mux.HandleFunc("/osewa", func(w http.ResponseWriter, r *http.Request) {
		id := ids.Add(1)
		opts := &websocket.AcceptOptions{Subprotocols: []string{subprotocol}}
		switch mode {
		case "ctx":
			opts.CompressionMode = websocket.CompressionContextTakeover
		case "nosub":
			opts.Subprotocols = nil
			opts.CompressionMode = websocket.CompressionContextTakeover
		}
		ws, err := websocket.Accept(w, r, opts)
		if err != nil {
			fmt.Println("accept", id, err)
			return
		}
		defer ws.CloseNow()
		ws.SetReadLimit(maxFrame)
		ctx := context.Background()
		for {
			typ, b, err := ws.Read(ctx)
			if err != nil {
				fmt.Println("end", id, int(websocket.CloseStatus(err)), err)
				return
			}
			if typ != websocket.MessageBinary {
				fmt.Println("text", id)
				continue
			}
			switch string(b) {
			case "ping-me":
				go func() {
					ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
					defer cancel()
					if err := ws.Ping(ctx); err != nil {
						fmt.Println("ping", id, err)
						return
					}
					ws.Write(ctx, websocket.MessageBinary, []byte("pinged"))
				}()
			case "close-me":
				fmt.Println("closed", id, ws.Close(4001, "go bye"))
				return
			case "text-me":
				ws.Write(ctx, websocket.MessageText, []byte("hello"))
			default:
				if err := ws.Write(ctx, websocket.MessageBinary, b); err != nil {
					fmt.Println("write", id, err)
					return
				}
			}
		}
	})
	return http.Serve(ln, mux)
}

// bench sends data as OUTPUT frames from a coder/websocket server (as
// osewa's) to a client (as osewa's) over loopback and prints the payload
// and wire bytes and the seconds it took.
func bench(data string) error {
	b, err := os.ReadFile(data)
	if err != nil {
		return err
	}
	fs := frames(b)
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return err
	}
	go http.Serve(ln, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ws, err := websocket.Accept(w, r, &websocket.AcceptOptions{
			Subprotocols:    []string{subprotocol},
			CompressionMode: websocket.CompressionContextTakeover,
		})
		if err != nil {
			return
		}
		defer ws.CloseNow()
		// Nothing goes before the client counts.
		if _, _, err := ws.Read(context.Background()); err != nil {
			return
		}
		for _, f := range fs {
			if err := ws.Write(context.Background(), websocket.MessageBinary, f); err != nil {
				return
			}
		}
		ws.Read(context.Background())
	}))
	ctx := context.Background()
	var read atomic.Int64
	ws, err := dial(ctx, ln.Addr().String(), "osewa", []string{subprotocol}, &read)
	if err != nil {
		return err
	}
	defer ws.CloseNow()
	before := read.Load()
	start := time.Now()
	if err := ws.Write(ctx, websocket.MessageBinary, []byte("go")); err != nil {
		return err
	}
	payload := 0
	for range fs {
		_, m, err := ws.Read(ctx)
		if err != nil {
			return err
		}
		payload += len(m)
	}
	elapsed := time.Since(start)
	fmt.Printf("bench %d %d %f\n", payload, read.Load()-before, elapsed.Seconds())
	return nil
}
