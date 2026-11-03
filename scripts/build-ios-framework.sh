#!/usr/bin/env bash
# Builds target/Osewa.xcframework: the C API (crates/ffi/include/osewa.h)
# as a dynamic framework for iOS devices and the arm64 simulator.
#
# Dynamic, exporting only the osewa_* functions of osewa.h: an app may
# link a libghostty of its own (another commit, the same ghostty_*
# names), and this one must stay private to the framework.
#
# MIN_IOS sets the deployment target (default 18.0, as the Moshi app).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
MIN_IOS=${MIN_IOS:-18.0}
out="$root/target/Osewa.xcframework"
work="$root/target/ios-framework"
header="$root/crates/ffi/include/osewa.h"
version=$(sed -nE 's/^version = "([^"]+)"/\1/p' Cargo.toml | head -1)

rm -rf "$work" "$out"
mkdir -p "$work"

# The header's function declarations are the exports: nothing else leaves
# the framework, not even the osewa_rs_* callbacks libghostty's bridge
# calls (rustc would export those from a cdylib).
sed -nE 's/^[a-z].*[ *](osewa_[a-z_]+)\(.*/_\1/p' "$header" | sort -u >"$work/exports.txt"
[[ -s "$work/exports.txt" ]] || { echo "no functions found in $header" >&2; exit 1; }

# The static library from cargo, linked by clang into the framework's
# binary: so the exports list is the only one the linker sees.
build_slice() {
	local rust_target=$1 zig_target=$2 sdk=$3 clang_target=$4 platform=$5
	# Sized for an app download, not for a host: libghostty ReleaseSmall
	# (in its own cache; still ~165 MB/s of parsing, far past any link),
	# Rust at opt-level s with fat LTO. Together ~1.9 MB off the device
	# slice's 5.3 MB.
	local lib="$root/.cache/libghostty-small/$zig_target"
	OPTIMIZE=ReleaseSmall LIBGHOSTTY_OUT=$lib TARGET=$zig_target ./scripts/libghostty.sh >/dev/null
	# Unwinding, so a panic is caught at the C boundary rather than taking
	# the app down; line tables, for the dSYM.
	OSEWA_LIBGHOSTTY=$lib IPHONEOS_DEPLOYMENT_TARGET=$MIN_IOS \
		CARGO_PROFILE_RELEASE_PANIC=unwind CARGO_PROFILE_RELEASE_DEBUG=line-tables-only \
		CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=fat \
		cargo rustc -p osewa-ffi --release --target "$rust_target" --crate-type staticlib >&2
	local fw="$work/$rust_target/Osewa.framework"
	mkdir -p "$fw/Headers" "$fw/Modules"
	xcrun --sdk "$sdk" clang -dynamiclib -target "$clang_target" -isysroot "$(xcrun --sdk "$sdk" --show-sdk-path)" \
		-o "$fw/Osewa" \
		-Wl,-install_name,@rpath/Osewa.framework/Osewa \
		-Wl,-exported_symbols_list,"$work/exports.txt" \
		-Wl,-dead_strip \
		-compatibility_version 1 -current_version "$version" \
		"target/$rust_target/release/libosewa_ffi.a" -liconv -lm
	xcrun dsymutil "$fw/Osewa" -o "$work/$rust_target/Osewa.framework.dSYM" 2>/dev/null
	xcrun strip -x "$fw/Osewa"
	cp "$header" "$fw/Headers/osewa.h"
	cat >"$fw/Modules/module.modulemap" <<-'EOF'
		framework module Osewa {
		  umbrella header "osewa.h"
		  export *
		}
	EOF
	cat >"$fw/Info.plist" <<-EOF
		<?xml version="1.0" encoding="UTF-8"?>
		<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
		<plist version="1.0">
		<dict>
		  <key>CFBundleDevelopmentRegion</key><string>en</string>
		  <key>CFBundleExecutable</key><string>Osewa</string>
		  <key>CFBundleIdentifier</key><string>dev.osewa.Osewa</string>
		  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
		  <key>CFBundleName</key><string>Osewa</string>
		  <key>CFBundlePackageType</key><string>FMWK</string>
		  <key>CFBundleShortVersionString</key><string>$version</string>
		  <key>CFBundleVersion</key><string>$version</string>
		  <key>CFBundleSupportedPlatforms</key><array><string>$platform</string></array>
		  <key>MinimumOSVersion</key><string>$MIN_IOS</string>
		</dict>
		</plist>
	EOF
	plutil -lint "$fw/Info.plist" >/dev/null
	# Exactly the header's functions, or the private libghostty leaks.
	if ! diff <(nm -gU "$fw/Osewa" | awk '{print $3}' | sort -u) "$work/exports.txt"; then
		echo "$fw/Osewa exports more or less than osewa.h declares" >&2
		exit 1
	fi
}

build_slice aarch64-apple-ios aarch64-ios iphoneos "arm64-apple-ios$MIN_IOS" iPhoneOS
build_slice aarch64-apple-ios-sim aarch64-ios-simulator iphonesimulator "arm64-apple-ios$MIN_IOS-simulator" iPhoneSimulator

xcodebuild -create-xcframework \
	-framework "$work/aarch64-apple-ios/Osewa.framework" -debug-symbols "$work/aarch64-apple-ios/Osewa.framework.dSYM" \
	-framework "$work/aarch64-apple-ios-sim/Osewa.framework" -debug-symbols "$work/aarch64-apple-ios-sim/Osewa.framework.dSYM" \
	-output "$out" >&2
echo "$out"
