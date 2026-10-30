// Command goparity writes the Go bench package's workloads and transfer
// pattern, each as an 8-byte little-endian length and the bytes, for the
// Rust port to compare with its own.
package main

import (
	"bufio"
	"encoding/binary"
	"io"
	"os"

	"github.com/rjyo/osewa/bench"
)

func main() {
	w := bufio.NewWriter(os.Stdout)
	defer w.Flush()
	put := func(b []byte) {
		binary.Write(w, binary.LittleEndian, uint64(len(b)))
		w.Write(b)
	}
	put(bench.W1(2000))
	put(bench.W2(2000, 80, 24))
	put(bench.W2(2000, 200, 60))
	put(bench.W2(450, 120, 40))
	put(bench.W3(2000))
	put(bench.W4(2*bench.W4Switch+7, 80, 24))
	put(bench.W4(60, 200, 60))
	for _, n := range []int64{0, 1, 7, 8, 9, 1000, 1<<20 + 3} {
		b, _ := io.ReadAll(bench.Pattern(n))
		put(b)
	}
}
