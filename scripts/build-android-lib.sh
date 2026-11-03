#!/usr/bin/env bash
# Builds target/android/<abi>/libosewa.so: the C API (crates/ffi/include/osewa.h)
# as a shared library for Android apps, the Android twin of
# scripts/build-ios-framework.sh.
#
# Shared, exporting only the osewa_* functions of osewa.h: the app loads a
# libghostty of its own (another commit, the same ghostty_* names), and this
# one must stay private to the library.
#
# Left unstripped (line tables only): the app's Gradle build strips the copy
# it packages and lifts the debug info into the bundle for Play.
#
# Usage: build-android-lib.sh [--abi arm64-v8a|x86_64]... [--all-abis]
# ANDROID_NDK_HOME picks the NDK (default: the SDK's 27.1.12297006, React
# Native's pin); ANDROID_API the minimum API level (default 24).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
ANDROID_API=${ANDROID_API:-24}
header="$root/crates/ffi/include/osewa.h"
work="$root/target/android-lib"

abis=()
while [[ $# -gt 0 ]]; do
	case "$1" in
	--abi) abis+=("$2"); shift 2 ;;
	--all-abis) abis=(arm64-v8a x86_64); shift ;;
	*) echo "unknown option: $1" >&2; exit 1 ;;
	esac
done
[[ ${#abis[@]} -gt 0 ]] || abis=(arm64-v8a)

ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-$HOME/Library/Android/sdk/ndk/27.1.12297006}}
case "$(uname -s)" in
Darwin) host_tag=darwin-x86_64 ;;
Linux) host_tag=linux-x86_64 ;;
*) echo "unsupported host: $(uname -s)" >&2; exit 1 ;;
esac
toolchain="$ndk/toolchains/llvm/prebuilt/$host_tag"
[[ -d $toolchain ]] || { echo "Android NDK toolchain not found: $toolchain (set ANDROID_NDK_HOME)" >&2; exit 1; }

mkdir -p "$work"
# The header's function declarations are the exports; nothing else leaves
# the library, not even the osewa_rs_* callbacks libghostty's bridge calls.
sed -nE 's/^[a-z].*[ *](osewa_[a-z_]+)\(.*/\1/p' "$header" | sort -u >"$work/exports.txt"
[[ -s "$work/exports.txt" ]] || { echo "no functions found in $header" >&2; exit 1; }
{
	echo "{"
	echo "  global:"
	sed 's/.*/    &;/' "$work/exports.txt"
	echo "  local: *;"
	echo "};"
} >"$work/exports.map"

build_abi() {
	local abi=$1 rust_target
	case "$abi" in
	arm64-v8a) rust_target=aarch64-linux-android ;;
	x86_64) rust_target=x86_64-linux-android ;;
	*) echo "unsupported ABI: $abi" >&2; exit 1 ;;
	esac
	local env_target=${rust_target//-/_}
	local upper_target
	upper_target=$(echo "$env_target" | tr '[:lower:]' '[:upper:]')
	local cc="$toolchain/bin/$rust_target$ANDROID_API-clang"

	# Sized for an app download, as the iOS framework: libghostty
	# ReleaseSmall in its own cache, Rust at opt-level s with fat LTO.
	local lib="$root/.cache/libghostty-small/$rust_target"
	OPTIMIZE=ReleaseSmall LIBGHOSTTY_OUT=$lib TARGET=$rust_target ./scripts/libghostty.sh >/dev/null
	# Unwinding, so a panic is caught at the C boundary rather than taking
	# the app down.
	env "CC_$env_target=$cc" "AR_$env_target=$toolchain/bin/llvm-ar" \
		"CARGO_TARGET_${upper_target}_LINKER=$cc" \
		OSEWA_LIBGHOSTTY="$lib" \
		CARGO_PROFILE_RELEASE_PANIC=unwind CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
		CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=fat \
		cargo rustc -p osewa-ffi --release --target "$rust_target" --crate-type staticlib >&2

	local out="$root/target/android/$abi"
	mkdir -p "$out"
	# -u keeps each export's archive member; the version script hides the
	# rest; 16 KB pages, as Android 15+ devices require.
	local undefs=()
	while read -r sym; do undefs+=("-Wl,-u,$sym"); done <"$work/exports.txt"
	"$cc" -shared -o "$out/libosewa.so" \
		-Wl,-soname,libosewa.so \
		-Wl,--version-script="$work/exports.map" \
		-Wl,--gc-sections \
		-Wl,--build-id=sha1 \
		-Wl,-z,max-page-size=16384 \
		-Wl,-z,noexecstack \
		"${undefs[@]}" \
		"target/$rust_target/release/libosewa_ffi.a" -llog -ldl -lm

	# Exactly the header's functions, or the private libghostty leaks.
	if ! diff <("$toolchain/bin/llvm-nm" -D --defined-only "$out/libosewa.so" | awk '{print $3}' | sed 's/@.*//' | sort -u) "$work/exports.txt"; then
		echo "$out/libosewa.so exports more or less than osewa.h declares" >&2
		exit 1
	fi
	cp "$header" "$root/target/android/osewa.h"
	echo "$out/libosewa.so"
}

for abi in "${abis[@]}"; do
	build_abi "$abi"
done
