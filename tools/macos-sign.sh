#!/bin/bash
# Sign, notarise and staple the macOS plugin bundles and zip the release folders
# that hold them, on a real Mac over ssh (`make release-macos`). Codesign and
# notarytool exist only on macOS, so the staged folders and the notary key are
# copied to a temporary directory on the Mac, tools/macos-sign-remote.sh does the
# work there, and the zips come back. They are made on the Mac with ditto,
# because a bundle zipped on Linux can lose what its signature covers. The
# temporary directory, key included, is removed from the Mac however the run
# ends.
#
# Usage: tools/macos-sign.sh <stage dir> <output dir>
# Every folder in the stage dir becomes <output dir>/<folder>.zip, after every
# .vst3 / .clap bundle directly inside it is signed, notarised and stapled.
# Machine, identity and key come from $MAC_SIGN_ENV (packaging/macos/sign.env,
# gitignored): MAC_SSH, SIGN_IDENTITY, NOTARY_KEY_FILE, NOTARY_KEY_ID and, for a
# Team API key, NOTARY_ISSUER.
set -euo pipefail

STAGE=$1
OUT=$2
ENV=${MAC_SIGN_ENV:-packaging/macos/sign.env}
HERE=$(cd "$(dirname "$0")" && pwd)

[ -f "$ENV" ] || {
	echo "error: $ENV not found; create it with MAC_SSH, SIGN_IDENTITY, NOTARY_KEY_FILE," >&2
	echo "       NOTARY_KEY_ID and NOTARY_ISSUER (see the top of $0)" >&2
	exit 1; }
. "$ENV"
for v in MAC_SSH SIGN_IDENTITY NOTARY_KEY_FILE NOTARY_KEY_ID; do
	[ -n "${!v:-}" ] || { echo "error: $v is not set in $ENV" >&2; exit 1; }
done
[ -f "$NOTARY_KEY_FILE" ] || { echo "error: notary key $NOTARY_KEY_FILE not found" >&2; exit 1; }
[ -d "$STAGE" ] || { echo "error: no stage dir $STAGE" >&2; exit 1; }

CONF=$(mktemp)
REMOTE=$(ssh "$MAC_SSH" 'mktemp -d -t lesynth-fourier-sign')
trap 'rm -f "$CONF"; ssh "$MAC_SSH" "rm -rf $REMOTE"' EXIT
{
	printf 'SIGN_IDENTITY=%q\n' "$SIGN_IDENTITY"
	printf 'NOTARY_KEY_ID=%q\n' "$NOTARY_KEY_ID"
	printf 'NOTARY_ISSUER=%q\n' "${NOTARY_ISSUER:-}"
} > "$CONF"

echo "── copying to the Mac ──────────────────────────────────────────────"
ssh "$MAC_SSH" "mkdir $REMOTE/stage"
scp -q -r "$STAGE"/* "$MAC_SSH:$REMOTE/stage/"
scp -q "$HERE/macos-sign-remote.sh" "$MAC_SSH:$REMOTE/sign.sh"
scp -q "$CONF" "$MAC_SSH:$REMOTE/sign.conf"
scp -q "$NOTARY_KEY_FILE" "$MAC_SSH:$REMOTE/AuthKey.p8"
ssh "$MAC_SSH" "chmod 600 $REMOTE/AuthKey.p8"

# -t, so that Ctrl-C here also stops the remote side.
ssh -t "$MAC_SSH" "/bin/bash $REMOTE/sign.sh"

mkdir -p "$OUT"
for d in "$STAGE"/*/; do
	name=$(basename "$d")
	scp -q "$MAC_SSH:$REMOTE/out/$name.zip" "$OUT/"
	echo "Signed + notarised: $OUT/$name.zip"
done
