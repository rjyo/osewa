#!/usr/bin/env bash
# Host binaries for moshi-hook to download: osewa for linux and darwin,
# amd64 and arm64, under one build id.
#
#   scripts/release.sh build     dist/<buildId>/, uploading nothing
#   scripts/release.sh publish   build unless dist/ has this commit's, then
#                                upload to R2 osewa/<buildId>/
#
# The build id is p<protocol version>-s<snapshot version>, read from each
# binary's `osewa version --json`: binaries with the same id interoperate,
# so a client picks one by id alone. dist/<buildId>/
# holds osewa-<os>-<arch>.gz (a gzip of the bare executable), SHA256SUMS
# over the .gz files, and build.json (the `version --json` object).
#
# Each binary must be the right arch per `file` (Linux: static) and carry
# the build id marker (osewa_client::BUILD_ID_MARKER). Those that can run
# here also print `version --json`, which must be identical: natively,
# darwin-amd64 under Rosetta, linux in Docker (plus a holder start). The
# rest get only the static checks, and the build says which got which.
#
# Linux binaries are static musl (cargo-zigbuild), so they run on any
# distribution. Darwin binaries target macOS 13 and are signed with the
# hardened runtime and notarized by scripts/sign-darwin.sh, which reads
# MACOS_SIGN_IDENTITY and NOTARY_PROFILE (or ASC_*) from the environment.
#
#   SKIP_MACOS_SIGN=1        unsigned darwin binaries (local dry runs)
#   SKIP_MACOS_NOTARIZE=1    signed, not notarized (local dry runs)
#   OSEWA_VERSION            the release name `version --json` reports as "osewa"
#
# publish uploads with AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
# R2_S3_ENDPOINT and R2_PUBLIC_URL from the environment. It refuses
# unsigned or unnotarized darwin binaries. Every object is uploaded
# create-only, and one already there must hold the same bytes: a published
# path never changes.
set -euo pipefail
# build runs in $(...), which must stop at the first failure too.
shopt -s inherit_errexit

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

mode=${1:-}
case $mode in
build | publish) ;;
*)
	echo "usage: scripts/release.sh build|publish" >&2
	exit 2
	;;
esac

bucket=moshi-ota
prefix=osewa
targets="linux-amd64 linux-arm64 darwin-amd64 darwin-arm64"
dist=$root/dist
work=$root/target/release-dist
libs=$root/.cache/release/libghostty
host="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')"

# The same zig as the Dockerfile pins, so a drifted local zig shows up here
# rather than in the binaries.
want_zig=0.17.0

# For each target: the Rust triple and the zig triple libghostty builds for.
triples() {
	case $1 in
	linux-amd64) echo x86_64-unknown-linux-musl x86_64-linux-musl ;;
	linux-arm64) echo aarch64-unknown-linux-musl aarch64-linux-musl ;;
	darwin-amd64) echo x86_64-apple-darwin x86_64-macos.13.0 ;;
	darwin-arm64) echo aarch64-apple-darwin aarch64-macos.13.0 ;;
	*) echo "release: unknown target $1" >&2; return 1 ;;
	esac
}

die() {
	echo "release: $*" >&2
	exit 1
}

# Build inputs only: other trees (site/) may be mid-edit in another session.
inputs=(crates Cargo.toml Cargo.lock .cargo scripts/libghostty.sh scripts/release.sh)
source_state() {
	local dirty=""
	[[ -n $(git status --porcelain -- "${inputs[@]}") ]] && dirty="-dirty"
	echo "$(git rev-parse HEAD)$dirty"
}

build_id() {
	python3 -I -c '
import json, sys
v = json.loads(sys.argv[1])
assert isinstance(v["version"], int) and isinstance(v["snapshot_version"], int), v
print("p%d-s%d" % (v["version"], v["snapshot_version"]))
' "$1"
}

# Whether a binary of target $1 runs here: natively, under Rosetta or in
# Docker.
runnable() {
	case $1 in
	"$host") return 0 ;;
	darwin-amd64) [[ $host == darwin-arm64 ]] && arch -x86_64 /usr/bin/true 2>/dev/null ;;
	linux-*) docker info >/dev/null 2>&1 ;;
	*) return 1 ;;
	esac
}

run_version() {
	local t=$1 bin=$2
	case $t in
	"$host") "$bin" version --json ;;
	darwin-amd64) arch -x86_64 "$bin" version --json ;;
	linux-*)
		# Debian 11's glibc is old; a static binary must not care.
		docker run --rm --platform "linux/${t#linux-}" -v "$(dirname "$bin"):/b:ro" \
			debian:bullseye-slim /b/osewa version --json
		;;
	esac
}

# What `file` must say about each target's binary.
file_wants() {
	case $1 in
	linux-amd64) echo "ELF 64-bit LSB executable, x86-64|statically linked" ;;
	linux-arm64) echo "ELF 64-bit LSB executable, ARM aarch64|statically linked" ;;
	darwin-amd64) echo "Mach-O 64-bit executable x86_64" ;;
	darwin-arm64) echo "Mach-O 64-bit executable arm64" ;;
	esac
}

check_file() {
	local t=$1 bin=$2 desc want w
	desc=$(file -b "$bin")
	IFS='|' read -ra want <<<"$(file_wants "$t")"
	for w in "${want[@]}"; do
		[[ $desc == *"$w"* ]] || die "$t: file says \"$desc\", want \"$w\""
	done
}

# The build id baked into a binary (osewa_client::BUILD_ID_MARKER), for the
# targets that can't run here: exactly one, or nothing.
marker_id() {
	local m
	m=$(LC_ALL=C grep -a -o 'osewa-build-id:p[0-9]*-s[0-9]*' "$1" | sort -u)
	[[ -n $m && $m != *$'\n'* ]] && echo "${m#osewa-build-id:}"
}

# A holder reaching "ready" has made a libghostty terminal and a PTY, which
# version never touches: the static musl build works past linking.
smoke_holder() {
	local t=$1 bin=$2 out
	out=$(docker run --rm --platform "linux/${t#linux-}" -v "$(dirname "$bin"):/b:ro" debian:bullseye-slim \
		sh -c 'mkdir -m 700 /tmp/run && timeout 10 /b/osewa holder --run /tmp/run --id smoke -- /bin/true | head -1' 2>&1) || true
	[[ $out == *ready* ]] || die "$t: holder smoke test failed: $out"
}

compile() {
	local t=$1 info rust zig lib
	info=$(triples "$t")
	read -r rust zig <<<"$info"
	lib=$libs/$t
	echo "release: $t ($rust)" >&2
	LIBGHOSTTY_OUT=$lib TARGET=$zig ./scripts/libghostty.sh >/dev/null
	local cargo=(cargo build)
	[[ $t == linux-* ]] && cargo=(cargo zigbuild)
	# Stripped, but not the proc macros cargo builds for the host: rustc
	# can't load those stripped.
	OSEWA_LIBGHOSTTY=$lib MACOSX_DEPLOYMENT_TARGET=13.0 \
		CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none \
		"${cargo[@]}" --release --locked -p osewa --target "$rust" --target-dir "$work/cargo" >&2
	mkdir -p "$work/stage/$t"
	cp "$work/cargo/$rust/release/osewa" "$work/stage/$t/osewa"
}

# Builds every target into dist/<buildId>/ and prints the build id.
build() {
	[[ $(zig version) == "$want_zig" ]] || die "zig $(zig version), want $want_zig"
	command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild missing (brew install cargo-zigbuild)"
	# Fail before the long compile, not after it.
	[[ ${SKIP_MACOS_SIGN:-} == 1 || -n ${MACOS_SIGN_IDENTITY:-} ]] ||
		die "set MACOS_SIGN_IDENTITY, or SKIP_MACOS_SIGN=1 for unsigned darwin binaries"

	local state t bin json id="" first=""
	state=$(source_state)
	rm -rf "$work/stage"
	local darwin=()
	for t in $targets; do
		compile "$t"
		[[ $t == darwin-* ]] && darwin+=("$work/stage/$t/osewa")
	done
	# One notarization for both arches: each submission waits minutes.
	[[ ${SKIP_MACOS_SIGN:-} == 1 ]] || ./scripts/sign-darwin.sh "${darwin[@]}"

	# Every target: the right arch (and static linking) per `file`, and the
	# marker's build id. Those that run here must also print the same
	# `version --json` (and linux holders start). With neither Rosetta nor
	# Docker, a target gets only the static checks.
	local ran=() static=()
	for t in $targets; do
		bin=$work/stage/$t/osewa
		check_file "$t" "$bin"
		runnable "$t" || { static+=("$t"); continue; }
		json=$(run_version "$t" "$bin") || die "$t: version --json failed"
		echo "release: $t ran: $json" >&2
		[[ $t == linux-* ]] && smoke_holder "$t" "$bin"
		if [[ -z $first ]]; then
			first=$json id=$(build_id "$json")
		elif [[ $json != "$first" ]]; then
			die "$t reports $json, not $first"
		fi
		ran+=("$t")
	done
	[[ -n $id ]] || die "no target runs here, so nothing reports a build id"
	for t in $targets; do
		bin=$work/stage/$t/osewa
		[[ $(marker_id "$bin") == "$id" ]] || die "$t: build id marker \"$(marker_id "$bin")\", want $id"
	done
	echo "release: ran version --json: ${ran[*]}; checked by file + marker only: ${static[*]:-none}" >&2

	local out=$dist/$id
	rm -rf "$out"
	mkdir -p "$out"
	for t in $targets; do
		# -n: no name or mtime in the header, so the same binary gzips the same.
		gzip -9 -n -c "$work/stage/$t/osewa" >"$out/osewa-$t.gz"
		gzip -dc "$out/osewa-$t.gz" | cmp -s - "$work/stage/$t/osewa" || die "$t: gzip round trip differs"
	done
	printf '%s\n' "$first" >"$out/build.json"
	(cd "$out" && shasum -a 256 osewa-*.gz >SHA256SUMS)

	local signed=0 notarized=0
	[[ ${SKIP_MACOS_SIGN:-} != 1 ]] && signed=1
	[[ $signed == 1 && ${SKIP_MACOS_NOTARIZE:-} != 1 ]] && notarized=1
	# Not uploaded (dotfile): what publish needs to reuse this build.
	printf 'source=%s\nsigned=%s\nnotarized=%s\n' "$state" "$signed" "$notarized" >"$out/.release"
	echo "$id"
}

# The dist/<buildId> built from this exact commit, signed and notarized.
reusable() {
	local state f
	state=$(source_state)
	[[ $state == *-dirty ]] && return 1
	for f in "$dist"/*/.release; do
		[[ -f $f ]] || continue
		grep -qx "source=$state" "$f" && grep -qx signed=1 "$f" && grep -qx notarized=1 "$f" || continue
		(cd "$(dirname "$f")" && shasum -a 256 -c --quiet SHA256SUMS) || continue
		basename "$(dirname "$f")"
		return 0
	done
	return 1
}

require_r2_env() {
	local k
	for k in AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY R2_S3_ENDPOINT R2_PUBLIC_URL; do
		[[ -n ${!k:-} ]] || die "$k is not set"
	done
}

s3() {
	aws --endpoint-url "$R2_S3_ENDPOINT" --region auto "$@"
}

# Create-only uploads (If-None-Match: *) arrived in aws-cli 2.23.
require_conditional_put() {
	local v
	v=$(aws --version 2>&1)
	[[ $v =~ aws-cli/([0-9]+)\.([0-9]+) ]] || die "can't read the aws CLI version: $v"
	((BASH_REMATCH[1] > 2 || (BASH_REMATCH[1] == 2 && BASH_REMATCH[2] >= 23))) ||
		die "aws CLI ${v%% *} can't upload create-only (put-object --if-none-match); need aws-cli >= 2.23"
}

# Whether $key/$1 already holds what dist has: 0 equal, 1 absent, and any
# other content is fatal, for a published path never changes.
same_remote() {
	local name=$1 got=$fetch/$1 err
	if err=$(s3 s3api get-object --bucket "$bucket" --key "$key/$name" "$got" 2>&1 >/dev/null); then
		cmp -s "$got" "$out/$name" || die "$key/$name is already published with other bytes; published paths never change"
		return 0
	fi
	grep -qE 'NoSuchKey|404|Not Found' <<<"$err" || die "can't read s3://$bucket/$key/$name: $err"
	return 1
}

# Uploads $1 only if nothing is at its key; if something got there first, it
# must be these bytes. So two publishers of one build id, or a resumed
# publish that rebuilt, can't replace an object anyone may have cached.
put_once() {
	local name=$1 type=$2 err
	if err=$(s3 s3api put-object --bucket "$bucket" --key "$key/$name" --body "$out/$name" \
		--content-type "$type" --cache-control "public, max-age=31536000, immutable" \
		--if-none-match '*' 2>&1 >/dev/null); then
		echo "release: uploaded $key/$name" >&2
		return
	fi
	grep -qE 'PreconditionFailed|412' <<<"$err" || die "uploading $key/$name: $err"
	same_remote "$name" || die "$key/$name: precondition failed but nothing is there: $err"
	echo "release: $key/$name was already there, the same" >&2
}

publish() {
	[[ ${SKIP_MACOS_SIGN:-} == 1 || ${SKIP_MACOS_NOTARIZE:-} == 1 ]] &&
		die "refusing to publish with SKIP_MACOS_SIGN or SKIP_MACOS_NOTARIZE: darwin binaries must be signed and notarized"
	[[ $(uname -s) == Darwin ]] || die "publish must run on macOS to sign and notarize the darwin binaries"
	[[ $(source_state) == *-dirty ]] &&
		die "uncommitted changes in build inputs: $(git status --porcelain -- "${inputs[@]}" | tr '\n' ' ')"
	command -v aws >/dev/null || die "aws CLI missing"
	require_conditional_put
	require_r2_env

	local id
	if id=$(reusable); then
		echo "release: reusing dist/$id" >&2
	else
		id=$(build)
	fi
	out=$dist/$id key=$prefix/$id
	grep -qx notarized=1 "$out/.release" || die "dist/$id is not notarized"
	fetch=$work/fetch
	rm -rf "$fetch"
	mkdir -p "$fetch"

	# SHA256SUMS goes last: its presence marks the build id published.
	local names=() types=() t i present=0
	for t in $targets; do
		names+=("osewa-$t.gz") types+=(application/gzip)
	done
	names+=(build.json SHA256SUMS) types+=(application/json "text/plain; charset=utf-8")

	# Everything already there must match before anything is added: a
	# resumed publish of other bytes stops here, not halfway.
	for i in "${!names[@]}"; do
		same_remote "${names[i]}" && present=$((present + 1))
	done
	if ((present == ${#names[@]})); then
		echo "release: $key already published with these binaries" >&2
		summary "$id"
		return
	fi
	for i in "${!names[@]}"; do
		put_once "${names[i]}" "${types[i]}"
	done

	local url=${R2_PUBLIC_URL%/}/$key
	[[ $(curl -fsS "$url/SHA256SUMS") == "$(cat "$out/SHA256SUMS")" ]] ||
		die "$url/SHA256SUMS doesn't match what was uploaded"
	echo "release: published $url/" >&2
	summary "$id"
}

summary() {
	echo "buildId: $1"
	cat "$dist/$1/SHA256SUMS"
}

case $mode in
build)
	id=$(build)
	summary "$id"
	;;
publish) publish ;;
esac
