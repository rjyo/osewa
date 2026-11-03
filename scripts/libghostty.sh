#!/usr/bin/env bash
# Builds the pinned libghostty-vt static library and headers into
# .cache/libghostty/<os>-<arch>. Host, holder and clients must all link the
# same commit: the snapshot format is not binary-stable yet.
#
# TARGET=<zig triple> (e.g. x86_64-linux-musl) cross-builds into
# .cache/libghostty/<os>-<arch> for that triple, where crates/ghostty/build.rs
# looks for that target.
#
# LIBGHOSTTY_OUT=<dir> builds there instead (cargo reads it as
# OSEWA_LIBGHOSTTY), so release builds for a baseline CPU never replace the
# development ones.
#
# OPTIMIZE=<zig mode> (default ReleaseFast); the iOS framework builds
# ReleaseSmall into its own LIBGHOSTTY_OUT.
set -euo pipefail

GHOSTTY_COMMIT=83fb95624b0439ee660c2679b43b38c8b63769cd
GHOSTTY_REPO=${GHOSTTY_REPO:-https://github.com/ghostty-org/ghostty.git}

root=$(cd "$(dirname "$0")/.." && pwd)
platform="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)"
zigflags=()
if [[ -n "${TARGET:-}" ]]; then
	arch=${TARGET%%-*} os=${TARGET#*-}
	os=${os%%[-.]*}
	# Match `uname` on macOS, as native builds' directory names do.
	[[ $os == macos ]] && os=darwin && arch=${arch/aarch64/arm64}
	platform="$os-$arch"
	[[ $TARGET == *simulator* ]] && platform="$platform-simulator"
	zigflags=("-Dtarget=$TARGET")
fi
src="$root/.cache/ghostty-src"
optimize=${OPTIMIZE:-ReleaseFast}
out=${LIBGHOSTTY_OUT:-"$root/.cache/libghostty/$platform"}

if [[ -f "$out/lib/libghostty-vt.a" && "$(cat "$out/COMMIT" 2>/dev/null)" == "$GHOSTTY_COMMIT" &&
	"$(cat "$out/TARGET" 2>/dev/null)" == "${TARGET:-}" &&
	"$(cat "$out/OPTIMIZE" 2>/dev/null || echo ReleaseFast)" == "$optimize" ]]; then
	echo "$out"
	exit 0
fi

if [[ ! -d "$src/.git" ]]; then
	git clone --quiet --filter=blob:none "$GHOSTTY_REPO" "$src"
fi
git -C "$src" fetch --quiet origin "$GHOSTTY_COMMIT" 2>/dev/null || true
git -C "$src" checkout --quiet --detach "$GHOSTTY_COMMIT"

rm -rf "$out"
(cd "$src" && zig build -Demit-lib-vt -Doptimize="$optimize" ${zigflags[@]+"${zigflags[@]}"} --prefix "$out" >&2)
cp -R "$src/include/ghostty" "$out/include/" 2>/dev/null || {
	mkdir -p "$out/include"
	cp -R "$src/include/ghostty" "$out/include/"
}
echo "$GHOSTTY_COMMIT" >"$out/COMMIT"
echo "${TARGET:-}" >"$out/TARGET"
echo "$optimize" >"$out/OPTIMIZE"
echo "$out"
