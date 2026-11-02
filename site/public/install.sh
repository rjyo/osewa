#!/bin/sh
# Installs osewa from its GitHub releases.
#
#   curl -fsSL https://osewa.dev/install.sh | sh
#   curl -fsSL https://osewa.dev/install.sh | OSEWA_VERSION=v0.1.0 sh
#   curl -fsSL https://osewa.dev/install.sh | OSEWA_INSTALL_DIR=/usr/local/bin sh
#
# Linux and macOS, amd64 and arm64. The download is checked against the
# release's SHA256SUMS before it replaces anything.
set -eu
# Whatever the caller's umask, nobody else may write the directory or the
# binary that will run as this user.
umask 022

repo=rjyo/osewa
dir=${OSEWA_INSTALL_DIR:-$HOME/.local/bin}
version=${OSEWA_VERSION:-}

err() {
	printf 'osewa install: %s\n' "$*" >&2
	exit 1
}

case $(uname -s) in
Linux) os=linux ;;
Darwin) os=darwin ;;
*) err "$(uname -s) isn't supported; osewa runs on Linux and macOS" ;;
esac
case $(uname -m) in
x86_64 | amd64) arch=amd64 ;;
aarch64 | arm64) arch=arm64 ;;
*) err "$(uname -m) isn't supported; osewa builds for amd64 and arm64" ;;
esac
# A shell under Rosetta reports x86_64 on an Apple silicon Mac.
if [ "$os/$arch" = darwin/amd64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
	arch=arm64
fi
asset=osewa-$os-$arch.gz

if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL --retry 2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -q -O "$2" "$1"; }
else
	err "needs curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
	err "needs sha256sum or shasum to check the download"
fi
command -v gzip >/dev/null 2>&1 || err "needs gzip"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t osewa)
trap 'rm -rf "$tmp"' EXIT INT TERM

# "latest" pinned to one release first: one that comes out between two
# downloads must not mix its SHA256SUMS with the last one's binary.
if [ -z "$version" ]; then
	fetch "https://github.com/$repo/releases/latest/download/build.json" "$tmp/build.json" || err "couldn't find the latest release"
	version=$(sed -n 's/.*"osewa":"\([0-9][0-9.]*\)".*/\1/p' "$tmp/build.json")
	[ -n "$version" ] || err "the latest release's build.json names no version"
fi
case $version in v*) ;; *) version=v$version ;; esac
base=https://github.com/$repo/releases/download/$version

printf 'downloading %s %s\n' "$asset" "$version"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || err "no release at $base"
fetch "$base/$asset" "$tmp/$asset" || err "couldn't download $base/$asset"
want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$want" ] || err "$asset isn't in the release's SHA256SUMS"
got=$(sha256 "$tmp/$asset")
[ "$got" = "$want" ] || err "$asset has SHA-256 $got, the release says $want"
gzip -dc "$tmp/$asset" >"$tmp/osewa"
chmod 755 "$tmp/osewa"
what=$("$tmp/osewa" version 2>/dev/null) || err "the downloaded osewa doesn't run here"

mkdir -p "$dir"
# Written beside the old binary under a name nobody can guess or plant,
# then renamed over it: a server starting meanwhile runs the old build or
# the new one, never half of one.
new=$(mktemp "$dir/.osewa.XXXXXX") || err "can't write to $dir"
cp "$tmp/osewa" "$new" && chmod 755 "$new" && mv -f "$new" "$dir/osewa" || {
	rm -f "$new"
	err "couldn't install into $dir"
}
printf 'installed %s at %s\n' "$what" "$dir/osewa"

case ":$PATH:" in
*":$dir:"*) ;;
*) printf '\n%s is not on your PATH; add it, e.g. to ~/.profile:\n  export PATH="%s:$PATH"\n' "$dir" "$dir" ;;
esac
# A running server keeps the old build until it restarts.
if pgrep -u "$(id -u)" -f "$dir/osewa\"? \"?server" >/dev/null 2>&1; then
	printf '\nan osewa server is running here; restart it to use this build:\n  osewa service restart\n'
fi
