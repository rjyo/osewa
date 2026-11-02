.PHONY: libghostty build check test cover clean ios-framework release release-publish

CARGO ?= cargo

libghostty:
	./scripts/libghostty.sh >/dev/null

build: libghostty
	$(CARGO) build --release
	mkdir -p bin && cp target/release/osewa bin/osewa

# target/Osewa.xcframework: the C API for iOS apps (macOS with Xcode).
ios-framework:
	./scripts/build-ios-framework.sh

# dist/vX.Y.Z/: osewa for linux and darwin, amd64 and arm64, gzipped with
# SHA256SUMS and build.json; see scripts/release.sh. release-publish tags
# HEAD and publishes them as a GitHub release.
release:
	./scripts/release.sh build

release-publish:
	./scripts/release.sh publish

check: libghostty
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test

test: check

clean:
	$(CARGO) clean
	rm -rf bin
