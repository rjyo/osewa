.PHONY: libghostty build check test cover clean ios-framework

CARGO ?= cargo

libghostty:
	./scripts/libghostty.sh >/dev/null

build: libghostty
	$(CARGO) build --release
	mkdir -p bin && cp target/release/osewa bin/osewa

# target/Osewa.xcframework: the C API for iOS apps (macOS with Xcode).
ios-framework:
	./scripts/build-ios-framework.sh

check: libghostty
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test

test: check

clean:
	$(CARGO) clean
	rm -rf bin
