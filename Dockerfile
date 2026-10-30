# Rust osewa for the e2e rig (Linux, the build platform's architecture).
#
#   docker build --target binary .                 /osewa and /osewa-bench
#   docker build --build-arg GO_IMAGE=<go e2e image> --target mixed .
#       the Go image with the Rust binary at /usr/local/bin/osewa-rs and
#       OSEWA_HOLDER pointing at it: a Go server spawning Rust holders
#       (needs a Go server that honors OSEWA_HOLDER).
#   docker build --build-arg GO_IMAGE=<go e2e image> --target rust .
#       the Go image with the Rust osewa and osewa-bench as
#       /usr/local/bin/osewa and /usr/local/bin/osewa-bench.
#
# Supply chain: the Rust image is pinned by digest and the Zig tarball by
# SHA-256, as in the Go Dockerfile.

ARG RUST_IMAGE=rust:1.93.1-bookworm@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31
ARG GO_IMAGE=osewa-e2e:dev

FROM ${RUST_IMAGE} AS toolchain
ARG TARGETARCH
ARG ZIG_VERSION=0.17.0
ARG ZIG_SHA256_X86_64=1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026
ARG ZIG_SHA256_AARCH64=9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8
RUN set -eu; \
	case "$TARGETARCH" in \
		arm64) zarch=aarch64; zigsum=$ZIG_SHA256_AARCH64 ;; \
		amd64) zarch=x86_64; zigsum=$ZIG_SHA256_X86_64 ;; \
		*) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
	esac; \
	cd /tmp; \
	curl -fsSLo zig.txz "https://ziglang.org/download/${ZIG_VERSION}/zig-${zarch}-linux-${ZIG_VERSION}.tar.xz"; \
	printf '%s  zig.txz\n' "$zigsum" | sha256sum -c -; \
	tar -xJf zig.txz -C /opt; \
	rm zig.txz; \
	ln -s "/opt/zig-${zarch}-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig

# Keyed only on the script, so code edits never rebuild libghostty.
FROM toolchain AS libghostty
WORKDIR /src
COPY scripts/libghostty.sh scripts/libghostty.sh
RUN --mount=type=cache,target=/root/.cache/zig \
	./scripts/libghostty.sh && rm -rf .cache/ghostty-src

FROM libghostty AS build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
	--mount=type=cache,target=/src/target \
	cargo build --release -p osewa -p osewa-bench \
	&& mkdir -p /out && cp target/release/osewa target/release/osewa-bench /out/

FROM scratch AS binary
COPY --from=build /out/osewa /out/osewa-bench /

FROM ${GO_IMAGE} AS mixed
COPY --from=build /out/osewa /usr/local/bin/osewa-rs
ENV OSEWA_HOLDER=/usr/local/bin/osewa-rs

# The Go image with the Rust osewa in its place: a Rust server spawning
# Rust holders, driven by the Go e2e harness and Go clients. osewa-bench
# stays Go's: it is part of the measuring harness, which must be the same
# for both sides of a comparison (docs/RUST.md, Rules).
FROM ${GO_IMAGE} AS rust
COPY --from=build /out/osewa /usr/local/bin/

# rust plus the Rust osewa-bench, for functional e2e runs of everything
# Rust (crates/bench/tests/go_parity.rs checks the formats and bytes).
FROM rust AS rust-all
COPY --from=build /out/osewa-bench /usr/local/bin/
