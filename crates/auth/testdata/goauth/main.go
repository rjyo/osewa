// Command goauth drives the Go osewa auth package for the Rust interop
// tests (crates/auth/tests/go_interop.rs). It is built in a temporary module
// that points github.com/rjyo/osewa at the Go checkout.
package main

import (
	"bufio"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/tls"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"time"

	"github.com/rjyo/osewa/auth"
)

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "goauth:", err)
		os.Exit(1)
	}
}

func run(args []string) error {
	switch args[0] {
	case "fixtures":
		return fixtures(args[1])
	case "check":
		return check(args[1])
	case "server":
		return server(args[1], args[2])
	case "client":
		return client(args[1], args[2], args[3], args[4])
	}
	return fmt.Errorf("unknown mode %q", args[0])
}

func randKey() string {
	pub, _, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		panic(err)
	}
	return auth.EncodeKey(pub)
}

// fixtures writes a key and devices files as Go writes them: server.key,
// server.pub, devices.full (three devices, one with HTML-escaped characters)
// and devices.removed (the same after removing "laptop").
func fixtures(dir string) error {
	id, err := auth.LoadOrCreateIdentity(filepath.Join(dir, "server.key"))
	if err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(dir, "server.pub"), []byte(auth.EncodeKey(id.Public())+"\n"), 0o644); err != nil {
		return err
	}
	d := &auth.Devices{Path: filepath.Join(dir, "devices")}
	for _, name := range []string{"phone", "laptop", "jyo's <iPad> & co "} {
		if err := d.Add(auth.Device{Name: name, Key: randKey()}); err != nil {
			return err
		}
	}
	if err := os.Rename(d.Path, d.Path+".full"); err != nil {
		return err
	}
	b, _ := os.ReadFile(d.Path + ".full")
	if err := os.WriteFile(d.Path, b, 0o600); err != nil {
		return err
	}
	if err := d.Remove("laptop"); err != nil {
		return err
	}
	return os.Rename(d.Path, d.Path+".removed")
}

// check loads dir/server.key and dir/devices and prints the public key and
// the device list as JSON, then enrolls a device "go" with the key in
// dir/go.pub.
func check(dir string) error {
	id, err := auth.LoadOrCreateIdentity(filepath.Join(dir, "server.key"))
	if err != nil {
		return err
	}
	d := &auth.Devices{Path: filepath.Join(dir, "devices")}
	list, err := d.List()
	if err != nil {
		return err
	}
	out, _ := json.Marshal(map[string]any{"key": auth.EncodeKey(id.Public()), "devices": list})
	fmt.Println(string(out))
	k, err := os.ReadFile(filepath.Join(dir, "go.pub"))
	if err != nil {
		return err
	}
	return d.Add(auth.Device{Name: "go", Key: string(k)})
}

// server prints its address, then for each connection prints "ok <client
// key>" or "err <reason>"; on ok it sends "hello\n" and waits for EOF.
func server(keyPath, devicesPath string) error {
	id, err := auth.LoadOrCreateIdentity(keyPath)
	if err != nil {
		return err
	}
	ln, err := tls.Listen("tcp", "127.0.0.1:0", auth.ServerTLS(id, &auth.Devices{Path: devicesPath}))
	if err != nil {
		return err
	}
	fmt.Println(ln.Addr())
	for {
		c, err := ln.Accept()
		if err != nil {
			return err
		}
		c.SetDeadline(time.Now().Add(5 * time.Second))
		s := c.(*tls.Conn)
		if err := s.Handshake(); err != nil {
			fmt.Println("err", err)
		} else {
			fmt.Println("ok", auth.EncodeKey(auth.PeerKey(s.ConnectionState())))
			s.Write([]byte("hello\n"))
			io.Copy(io.Discard, s)
		}
		s.Close()
	}
}

// client connects with keyPath's identity pinning serverKey and prints "ok
// <server's line>" or "err <reason>". maxVersion "1.2" offers only TLS 1.2.
func client(keyPath, serverKey, addr, maxVersion string) error {
	id, err := auth.LoadOrCreateIdentity(keyPath)
	if err != nil {
		return err
	}
	pin, err := auth.DecodeKey(serverKey)
	if err != nil {
		return err
	}
	cfg := auth.ClientTLS(id, pin)
	if maxVersion == "1.2" {
		cfg.MinVersion, cfg.MaxVersion = tls.VersionTLS12, tls.VersionTLS12
	}
	c, err := tls.DialWithDialer(&net.Dialer{Timeout: 5 * time.Second}, "tcp", addr, cfg)
	if err != nil {
		fmt.Println("err", err)
		return nil
	}
	defer c.Close()
	c.SetDeadline(time.Now().Add(5 * time.Second))
	line, err := bufio.NewReader(c).ReadString('\n')
	if err != nil {
		fmt.Println("err", err)
		return nil
	}
	fmt.Printf("ok %s", line)
	return nil
}
