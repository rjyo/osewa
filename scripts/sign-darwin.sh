#!/usr/bin/env bash
# Signs darwin osewa binaries in place with a Developer ID and the hardened
# runtime, then notarizes them in one submission.
#
#   scripts/sign-darwin.sh BIN...
#
#   MACOS_SIGN_IDENTITY   the codesigning identity in your keychain, e.g.
#                         "Developer ID Application: Example Ltd (TEAMID)"
#                         (security find-identity -v -p codesigning)
#   NOTARY_PROFILE        a notarytool keychain profile, made once with
#                         xcrun notarytool store-credentials NAME --key AuthKey_X.p8 --key-id X --issuer Y
#   or ASC_PRIVATE_KEY_PATH, ASC_KEY_ID, ASC_ISSUER_ID
#                         an App Store Connect API key, passed directly
#   SKIP_MACOS_NOTARIZE=1 sign only
#
# A bare Mach-O can't carry a stapled ticket; Gatekeeper looks the
# notarization up online instead.
set -euo pipefail

die() {
	echo "sign-darwin: $*" >&2
	exit 1
}

(($# > 0)) || die "usage: scripts/sign-darwin.sh BIN..."
[[ $(uname -s) == Darwin ]] || die "codesign and notarytool need macOS"
[[ -n ${MACOS_SIGN_IDENTITY:-} ]] ||
	die "set MACOS_SIGN_IDENTITY (security find-identity -v -p codesigning lists yours)"

notary=()
if [[ ${SKIP_MACOS_NOTARIZE:-} != 1 ]]; then
	if [[ -n ${NOTARY_PROFILE:-} ]]; then
		notary=(--keychain-profile "$NOTARY_PROFILE")
	elif [[ -n ${ASC_PRIVATE_KEY_PATH:-} && -n ${ASC_KEY_ID:-} && -n ${ASC_ISSUER_ID:-} ]]; then
		[[ -f $ASC_PRIVATE_KEY_PATH ]] || die "no $ASC_PRIVATE_KEY_PATH"
		notary=(--key "$ASC_PRIVATE_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID")
	else
		die "set NOTARY_PROFILE or ASC_PRIVATE_KEY_PATH/ASC_KEY_ID/ASC_ISSUER_ID, or SKIP_MACOS_NOTARIZE=1"
	fi
fi

for bin in "$@"; do
	# A linux binary handed here by mistake must not get a Mach-O signature.
	file -b "$bin" | grep -q Mach-O || die "$bin is not Mach-O"
	codesign --force --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$bin"
	codesign --verify --strict "$bin"
	[[ $(codesign -dv "$bin" 2>&1) == *flags=*'(runtime)'* ]] || die "$bin lacks the hardened runtime"
	echo "sign-darwin: signed $bin" >&2
done

((${#notary[@]})) || {
	echo "sign-darwin: SKIP_MACOS_NOTARIZE=1, not notarized" >&2
	exit 0
}

work=$(mktemp -d -t osewa-notary)
trap 'rm -rf "$work"' EXIT
i=0
for bin in "$@"; do
	# Several binaries share the name osewa; the zip needs distinct ones.
	mkdir -p "$work/stage/$i"
	cp "$bin" "$work/stage/$i/"
	i=$((i + 1))
done
ditto -c -k "$work/stage" "$work/notary.zip"
out=$(xcrun notarytool submit "$work/notary.zip" "${notary[@]}" --wait --output-format json)
echo "$out" >&2
[[ $out =~ \"status\"\ *:\ *\"Accepted\" ]] || die "notarization was not accepted (xcrun notarytool log <id> says why)"
echo "sign-darwin: notarized $*" >&2
