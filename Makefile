.PHONY: libghostty build check test cover clean

CARGO ?= cargo

libghostty:
	./scripts/libghostty.sh >/dev/null

build: libghostty
	$(CARGO) build --release
	mkdir -p bin && cp target/release/osewa bin/osewa

check: libghostty
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test

test: check

clean:
	$(CARGO) clean
	rm -rf bin
