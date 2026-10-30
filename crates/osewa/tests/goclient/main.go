// goclient drives an osewa server with the Go client library, for the
// Rust server's interop test: create, attach with a snapshot, echo, drop
// the connection, resume on a new one without a snapshot, and kill.
//
//	goclient <addr> <server key> <client key file>
package main

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/rjyo/osewa/auth"
	"github.com/rjyo/osewa/client"
	"github.com/rjyo/osewa/wire"
)

type output struct {
	mu        sync.Mutex
	buf       bytes.Buffer
	snapshots int
	ready     bool
}

func (o *output) has(s string) bool {
	o.mu.Lock()
	defer o.mu.Unlock()
	return o.ready && bytes.Contains(o.buf.Bytes(), []byte(s))
}

func waitFor(o *output, s string) error {
	for deadline := time.Now().Add(10 * time.Second); time.Now().Before(deadline); time.Sleep(10 * time.Millisecond) {
		if o.has(s) {
			return nil
		}
	}
	o.mu.Lock()
	defer o.mu.Unlock()
	return fmt.Errorf("no %q in %q (ready %v)", s, o.buf.String(), o.ready)
}

func main() {
	if err := run(os.Args[1], os.Args[2], os.Args[3]); err != nil {
		fmt.Fprintln(os.Stderr, "goclient:", err)
		os.Exit(1)
	}
	fmt.Println("ok")
}

func run(addr, serverKey, keyFile string) error {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	id, err := auth.LoadOrCreateIdentity(keyFile)
	if err != nil {
		return err
	}
	key, err := auth.DecodeKey(serverKey)
	if err != nil {
		return err
	}
	cfg := client.Config{URL: "wss://" + addr + wire.Path, TLS: auth.ClientTLS(id, key), ClientID: "go-interop", Carrier: "ws"}
	c, err := client.Dial(ctx, cfg)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	w, err := c.Welcome(ctx)
	if err != nil {
		return err
	}
	if w.Server != "rust" {
		return fmt.Errorf("welcome %+v", w)
	}
	sid, err := c.Create(ctx, client.CreateOptions{Cols: 80, Rows: 24, Argv: []string{"/bin/cat"}})
	if err != nil {
		return fmt.Errorf("create: %w", err)
	}
	o := &output{}
	opt := client.AttachOptions{
		Cols: 80, Rows: 24, Own: true,
		OnSnapshotBegin: func(wire.SnapshotBegin) { o.mu.Lock(); o.snapshots++; o.mu.Unlock() },
		OnReady:         func() { o.mu.Lock(); o.ready = true; o.mu.Unlock() },
		OnOutput:        func(b []byte) { o.mu.Lock(); o.buf.Write(b); o.mu.Unlock() },
	}
	s, err := c.Attach(ctx, sid, opt)
	if err != nil {
		return fmt.Errorf("attach: %w", err)
	}
	if s.Owner() != "go-interop" {
		return fmt.Errorf("owner %q", s.Owner())
	}
	if err := s.SendText("hello from go\r"); err != nil {
		return err
	}
	if err := waitFor(o, "hello from go\r\nhello from go"); err != nil {
		return err
	}
	// A new connection, as after a network change; the holder kept our
	// position, so the resume brings no snapshot.
	c.CloseNow()
	c2, err := client.Dial(ctx, cfg)
	if err != nil {
		return fmt.Errorf("redial: %w", err)
	}
	defer c2.Close()
	o.mu.Lock()
	before := o.snapshots
	o.mu.Unlock()
	if err := s.Resume(ctx, c2); err != nil {
		return fmt.Errorf("resume: %w", err)
	}
	if err := s.SendText("after resume\r"); err != nil {
		return err
	}
	if err := waitFor(o, "after resume\r\nafter resume"); err != nil {
		return err
	}
	o.mu.Lock()
	again := o.snapshots - before
	o.mu.Unlock()
	if again != 0 {
		return fmt.Errorf("resume sent %d snapshots", again)
	}
	list, err := c2.List(ctx)
	if err != nil || len(list) != 1 || list[0].ID != sid || list[0].Owner != "go-interop" {
		return fmt.Errorf("list %+v %v", list, err)
	}
	if err := c2.Kill(ctx, sid); err != nil {
		return fmt.Errorf("kill: %w", err)
	}
	select {
	case <-s.Done():
	case <-ctx.Done():
		return fmt.Errorf("session did not end")
	}
	if code, ok := s.ExitCode(); !ok || code != 129 {
		return fmt.Errorf("exit code %d %v", code, ok)
	}
	return nil
}
