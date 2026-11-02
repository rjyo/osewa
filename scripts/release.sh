#!/usr/bin/env bash
# Host binaries for moshi-hook to download: osewa for linux and darwin,
# amd64 and arm64, under one build id.
#
#   scripts/release.sh build     dist/<buildId>/, uploading nothing
#   scripts/release.sh publish   build unless dist/ has this commit's, then
#                                upload to R2 osewa/<buildId>/
#
# The build id is p<protocol version>-g<ghostty commit, 12>-s<snapshot
# version>, read from each binary's `osewa version --json`: binaries with
# the same id interoperate, so a client picks one by id alone. dist/<buildId>/
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
# distribution. Darwin binaries target macOS 13 and are codesigned with the
# hardened runtime and notarized by app-hook's sign-notarize-darwin.sh,
# which reads the same signing env (MACOS_SIGN_*, ASC_*; ASC_* falls back to
# marketing/asc/.env through the secrets store, so run munlock first).
#
#   SKIP_MACOS_SIGN=1        unsigned darwin binaries (local dry runs)
#   SKIP_MACOS_NOTARIZE=1    signed, not notarized (local dry runs)
#   APP_HOOK_DIR             app-hook checkout (default ../app-hook)
#   OSEWA_VERSION            the release name `version --json` reports as "osewa"
#
# publish reads AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, R2_S3_ENDPOINT and
# R2_PUBLIC_URL from app-hook's .env (or its encrypted copy). It refuses
# unsigned or unnotarized darwin binaries, and a build id whose published
# SHA256SUMS differ: a published path never changes.
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

APP_HOOK_DIR=${APP_HOOK_DIR:-$(cd "$root/.." && pwd)/app-hook}
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
assert len(v["ghostty"]) == 40, v
print("p%d-g%s-s%d" % (v["version"], v["ghostty"][:12], v["snapshot_version"]))
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
	m=$(LC_ALL=C grep -a -o 'osewa-build-id:p[0-9]*-g[0-9a-f]\{12\}-s[0-9]*' "$1" | sort -u)
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

sign() {
	local bin=$1
	"$APP_HOOK_DIR/scripts/sign-notarize-darwin.sh" "$bin" >&2
	[[ ${SKIP_MACOS_SIGN:-} == 1 ]] && return
	codesign --verify --strict "$bin"
	[[ $(codesign -dv "$bin" 2>&1) == *flags=*'(runtime)'* ]] || die "$bin lacks the hardened runtime"
}

# Builds every target into dist/<buildId>/ and prints the build id.
build() {
	[[ $(zig version) == "$want_zig" ]] || die "zig $(zig version), want $want_zig"
	command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild missing (brew install cargo-zigbuild)"
	if [[ ${SKIP_MACOS_SIGN:-} != 1 ]]; then
		[[ -x $APP_HOOK_DIR/scripts/sign-notarize-darwin.sh ]] ||
			die "no $APP_HOOK_DIR/scripts/sign-notarize-darwin.sh (set APP_HOOK_DIR)"
		# Fail rather than ship signed-but-unnotarized binaries silently.
		export REQUIRE_MACOS_NOTARIZE=1
	fi

	local state t bin json id="" first=""
	state=$(source_state)
	rm -rf "$work/stage"
	for t in $targets; do
		compile "$t"
		[[ $t == darwin-* ]] && sign "$work/stage/$t/osewa"
	done

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

load_r2_env() {
	# shellcheck disable=SC1091
	. "$APP_HOOK_DIR/scripts/env-lib.sh"
	local content line k v
	content=$(read_env_file "$APP_HOOK_DIR/.env") || die "no $APP_HOOK_DIR/.env (run munlock?)"
	# Only the keys an upload needs, without sourcing the rest. Split by
	# hand: `IFS== read` drops a value's trailing "=".
	while IFS= read -r line || [[ -n $line ]]; do
		k=${line%%=*} v=${line#*=}
		case $k in
		AWS_ACCESS_KEY_ID | AWS_SECRET_ACCESS_KEY | R2_S3_ENDPOINT | R2_PUBLIC_URL)
			v=${v%\"} v=${v#\"} v=${v%\'} v=${v#\'}
			[[ -z ${!k:-} ]] && export "$k=$v"
			;;
		esac
	done <<<"$content"
	for k in AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY R2_S3_ENDPOINT R2_PUBLIC_URL; do
		[[ -n ${!k:-} ]] || die "$k missing from $APP_HOOK_DIR/.env"
	done
}

s3() {
	aws --endpoint-url "$R2_S3_ENDPOINT" --region auto "$@"
}

publish() {
	[[ ${SKIP_MACOS_SIGN:-} == 1 || ${SKIP_MACOS_NOTARIZE:-} == 1 ]] &&
		die "refusing to publish with SKIP_MACOS_SIGN or SKIP_MACOS_NOTARIZE: darwin binaries must be signed and notarized"
	[[ $(uname -s) == Darwin ]] || die "publish must run on macOS to sign and notarize the darwin binaries"
	[[ $(source_state) == *-dirty ]] &&
		die "uncommitted changes in build inputs: $(git status --porcelain -- "${inputs[@]}" | tr '\n' ' ')"
	command -v aws >/dev/null || die "aws CLI missing"
	load_r2_env

	local id
	if id=$(reusable); then
		echo "release: reusing dist/$id" >&2
	else
		id=$(build)
	fi
	local out=$dist/$id key=$prefix/$id
	grep -qx notarized=1 "$out/.release" || die "dist/$id is not notarized"

	local head remote
	if head=$(s3 s3api head-object --bucket "$bucket" --key "$key/SHA256SUMS" 2>&1); then
		remote=$(s3 s3 cp "s3://$bucket/$key/SHA256SUMS" -)
		if [[ $remote == "$(cat "$out/SHA256SUMS")" ]]; then
			echo "release: $key already published with these binaries" >&2
			summary "$id"
			return
		fi
		printf 'published:\n%s\nlocal:\n%s\n' "$remote" "$(cat "$out/SHA256SUMS")" >&2
		die "$key is already published with other binaries; published paths never change"
	elif ! grep -qE '404|Not Found' <<<"$head"; then
		die "can't check s3://$bucket/$key/SHA256SUMS: $head"
	fi

	local immutable="public, max-age=31536000, immutable" t
	for t in $targets; do
		s3 s3 cp --only-show-errors --content-type application/gzip --cache-control "$immutable" \
			"$out/osewa-$t.gz" "s3://$bucket/$key/osewa-$t.gz"
	done
	s3 s3 cp --only-show-errors --content-type application/json --cache-control "$immutable" \
		"$out/build.json" "s3://$bucket/$key/build.json"
	# Last: its presence marks the build id published.
	s3 s3 cp --only-show-errors --content-type "text/plain; charset=utf-8" --cache-control "$immutable" \
		"$out/SHA256SUMS" "s3://$bucket/$key/SHA256SUMS"

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
