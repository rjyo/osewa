# osewa for Linux, the build platform's architecture: the binaries the e2e
# rig's images are built from.
#
#   docker build --target binary -o out .     out/osewa and out/osewa-bench
#   docker build --target binary -t osewa-binary .
#
# Supply chain: the Rust image is pinned by digest and the Zig tarball by
# SHA-256.

ARG RUST_IMAGE=rust:1.93.1-bookworm@sha256:7c4ae649a84014c467d79319bbf17ce2632ae8b8be123ac2fb2ea5be46823f31

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
