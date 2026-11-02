#!/usr/bin/env bash
# Releases osewa: linux and darwin, amd64 and arm64, as a GitHub release
# tagged vX.Y.Z.
#
#   scripts/release.sh notes [vX.Y.Z]    docs/releases/vX.Y.Z.md from the commits
#                                        since the last tag, to edit and commit
#   scripts/release.sh build [vX.Y.Z]    dist/vX.Y.Z/, publishing nothing
#   scripts/release.sh publish [vX.Y.Z]  build unless dist/ has this commit's,
#                                        then tag HEAD and create the release
#
# The version defaults to the patch after the latest vX.Y.Z tag (v0.1.0
# with none), and is built in as OSEWA_VERSION: `osewa version` prints it,
# and install.sh and auto-update compare it. dist/vX.Y.Z/ holds
# osewa-<os>-<arch>.gz (a gzip of the bare executable), SHA256SUMS over the
# .gz files, and build.json (the `version --json` object); those are the
# release's assets.
#
# The build id (p<protocol version>-s<snapshot version>) says which builds
# interoperate. Each binary must be the right arch per `file` (Linux:
# static) and carry the build id marker (osewa_client::BUILD_ID_MARKER).
# Those that can run here also print `version --json`, which must be
# identical and name this version: natively, darwin-amd64 under Rosetta,
# linux in Docker (plus a holder start). The rest get only the static
# checks, and the build says which got which.
#
# Linux binaries are static musl (cargo-zigbuild), so they run on any
# distribution. Darwin binaries target macOS 13 and are signed with the
# hardened runtime and notarized by scripts/sign-darwin.sh, which reads
# MACOS_SIGN_IDENTITY and NOTARY_PROFILE (or ASC_*) from the environment.
#
#   SKIP_MACOS_SIGN=1        unsigned darwin binaries (local dry runs)
#   SKIP_MACOS_NOTARIZE=1    signed, not notarized (local dry runs)
#
# publish needs gh, logged in with push access, HEAD on origin/main, and
# docs/releases/vX.Y.Z.md committed: those are the release notes. It
# refuses unsigned or unnotarized darwin binaries, and a version already
# released: a published version never changes.
set -euo pipefail
# macOS's /bin/bash is 3.2, which has no inherit_errexit.
((BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4))) || {
	echo "release: needs bash 4.4 or later (brew install bash), not $BASH_VERSION" >&2
	exit 1
}
# build runs in $(...), which must stop at the first failure too.
shopt -s inherit_errexit

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

mode=${1:-}
case $mode in
notes | build | publish) ;;
*)
	echo "usage: scripts/release.sh notes|build|publish [vX.Y.Z]" >&2
	exit 2
	;;
esac

repo=rjyo/osewa
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
inputs=(crates Cargo.toml Cargo.lock .cargo scripts/libghostty.sh scripts/release.sh scripts/sign-darwin.sh)
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
	OSEWA_VERSION=${version#v} OSEWA_LIBGHOSTTY=$lib MACOSX_DEPLOYMENT_TARGET=13.0 \
		CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none \
		"${cargo[@]}" --release --locked -p osewa --target "$rust" --target-dir "$work/cargo" >&2
	mkdir -p "$work/stage/$t"
	cp "$work/cargo/$rust/release/osewa" "$work/stage/$t/osewa"
}

latest_tag() {
	git tag --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true
}

# The version named on the command line, else a release tag already on
# HEAD (a publish to finish), else the patch after the latest tag, else
# v0.1.0.
resolve_version() {
	local v=${1:-} latest
	[[ -z $v ]] && v=$(git tag --points-at HEAD --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true)
	if [[ -z $v ]]; then
		latest=$(latest_tag)
		if [[ $latest =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
			v="v${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$((BASH_REMATCH[3] + 1))"
		else
			v=v0.1.0
		fi
	fi
	[[ $v == v* ]] || v=v$v
	[[ $v =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version $v is not vX.Y.Z"
	echo "$v"
}

notes() {
	local out=docs/releases/$version.md prev subjects
	[[ -f $out ]] && die "$out exists; edit it, or delete it to start over"
	prev=$(git tag --list 'v*' --sort=-v:refname | grep -vx "$version" | head -1 || true)
	mkdir -p docs/releases
	if [[ -z $prev ]]; then
		printf -- '- First release.\n' >"$out"
	else
		# Subjects are a starting point: rewrite them for people who use
		# osewa, and drop what they won't notice.
		subjects=$(git log --format=%s --reverse "$prev..HEAD")
		[[ -n $subjects ]] || die "no commits since $prev"
		sed 's/^/- /' <<<"$subjects" >"$out"
	fi
	echo "release: wrote $out${prev:+ from $prev..HEAD}; edit it and commit it before publish" >&2
}

# Builds every target into dist/<version>/.
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
	# `version --json`, naming this version (and linux holders start). With
	# neither Rosetta nor Docker, a target gets only the static checks.
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
			[[ $json == *"\"osewa\":\"${version#v}\""* ]] || die "$t reports $json, not osewa ${version#v}"
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

	local out=$dist/$version
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
	printf 'version=%s\nsource=%s\nsigned=%s\nnotarized=%s\nbuild=%s\n' "$version" "$state" "$signed" "$notarized" "$id" >"$out/.release"
}

# Whether dist/<version> was built as this version from this exact commit,
# signed and notarized, and is intact: every asset, and build.json naming
# this version.
reusable() {
	local d=$dist/$version f=$dist/$version/.release state t
	state=$(source_state)
	[[ $state != *-dirty && -f $f ]] || return 1
	grep -qx "version=$version" "$f" && grep -qx "source=$state" "$f" &&
		grep -qx signed=1 "$f" && grep -qx notarized=1 "$f" || return 1
	for t in $targets; do
		grep -q " osewa-$t.gz\$" "$d/SHA256SUMS" || return 1
	done
	[[ $(cat "$d/build.json" 2>/dev/null) == *"\"osewa\":\"${version#v}\""* ]] || return 1
	(cd "$d" && shasum -a 256 -c --quiet SHA256SUMS)
}

# What will be uploaded, checked whatever .release says: the darwin
# binaries signed with the hardened runtime and notarized, by Gatekeeper's
# own assessment.
verify_darwin() {
	local t bin chk=$work/verify
	rm -rf "$chk"
	mkdir -p "$chk"
	for t in darwin-amd64 darwin-arm64; do
		bin=$chk/osewa-$t
		gzip -dc "$dist/$version/osewa-$t.gz" >"$bin"
		codesign --verify --strict "$bin" 2>/dev/null || die "$t: not validly signed"
		[[ $(codesign -dv "$bin" 2>&1) == *flags=*'(runtime)'* ]] || die "$t: no hardened runtime"
		[[ $(spctl -a -vvv -t install "$bin" 2>&1) == *"source=Notarized Developer ID"* ]] || die "$t: Gatekeeper doesn't see it notarized"
	done
	rm -rf "$chk"
}

publish() {
	local notes_file=docs/releases/$version.md
	[[ ${SKIP_MACOS_SIGN:-} == 1 || ${SKIP_MACOS_NOTARIZE:-} == 1 ]] &&
		die "refusing to publish with SKIP_MACOS_SIGN or SKIP_MACOS_NOTARIZE: darwin binaries must be signed and notarized"
	[[ $(uname -s) == Darwin ]] || die "publish must run on macOS to sign and notarize the darwin binaries"
	command -v gh >/dev/null || die "gh missing (brew install gh)"
	gh auth status >/dev/null 2>&1 || die "gh is not logged in (gh auth login)"
	[[ $(source_state) == *-dirty ]] &&
		die "uncommitted changes in build inputs: $(git status --porcelain -- "${inputs[@]}" | tr '\n' ' ')"
	git ls-files --error-unmatch "$notes_file" >/dev/null 2>&1 && [[ -z $(git status --porcelain -- "$notes_file") ]] ||
		die "$notes_file isn't committed (scripts/release.sh notes $version writes one)"
	# The release must be a commit everyone can get.
	git fetch -q origin main
	git merge-base --is-ancestor HEAD origin/main || die "HEAD isn't on origin/main; push it first"
	local draft
	if draft=$(gh release view "$version" -R "$repo" --json isDraft -q .isDraft 2>/dev/null); then
		[[ $draft == true ]] || die "$version is already released; a published version never changes"
		# What an interrupted create leaves: nobody can see a draft, so it
		# goes, and the release is made again from dist.
		echo "release: deleting the unfinished draft of $version" >&2
		gh release delete "$version" -R "$repo" --yes
	fi
	if git rev-parse -q --verify "refs/tags/$version" >/dev/null; then
		[[ $(git rev-parse "$version^{commit}") == $(git rev-parse HEAD) ]] || die "tag $version exists on another commit"
	fi

	if reusable; then
		echo "release: reusing dist/$version" >&2
	else
		build
	fi
	local out=$dist/$version
	grep -qx notarized=1 "$out/.release" || die "dist/$version is not notarized"
	verify_darwin

	if ! git rev-parse -q --verify "refs/tags/$version" >/dev/null; then
		git tag -a "$version" -m "osewa $version"
	fi
	git push -q origin "refs/tags/$version"

	local assets=() t
	for t in $targets; do assets+=("$out/osewa-$t.gz"); done
	assets+=("$out/build.json" "$out/SHA256SUMS")
	gh release create "$version" "${assets[@]}" -R "$repo" --verify-tag --title "osewa $version" --notes-file "$notes_file"

	[[ $(gh release download "$version" -R "$repo" -p SHA256SUMS -O -) == "$(cat "$out/SHA256SUMS")" ]] ||
		die "the release's SHA256SUMS doesn't match dist/$version"
	echo "release: published https://github.com/$repo/releases/tag/$version" >&2
	summary
}

summary() {
	echo "osewa $version ($(sed -n 's/^build=//p' "$dist/$version/.release"))"
	cat "$dist/$version/SHA256SUMS"
}

version=$(resolve_version "${2:-}")
case $mode in
notes) notes ;;
build)
	build
	summary
	;;
publish) publish ;;
esac
