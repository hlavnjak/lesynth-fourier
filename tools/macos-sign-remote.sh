#!/bin/bash
# Runs ON the Mac, copied there by tools/macos-sign.sh with the staged release
# folders (stage/), sign.conf and the notary key, all in one temporary
# directory. Signs every plugin bundle, notarises them in one submission,
# staples each, and zips every release folder into out/. Written for the stock
# /bin/bash 3.2.
#
# codesign cannot use a private key from an ssh login: sshd puts it in the
# "Background" session, where every signature fails with errSecInternalComponent
# whatever keychain the key is in and whether or not it is unlocked. The logged-in
# desktop ("Aqua") session signs fine, and its login keychain is already
# unlocked. So, started over ssh, this script re-runs itself in that session as a
# one-shot launchd job and follows the job's log until it finishes.
set -euo pipefail
cd "$(dirname "$0")"
DIR=$(pwd)

if [ "${1:-}" != --in-session ]; then
	uid=$(id -u)
	launchctl print "gui/$uid" >/dev/null 2>&1 || {
		echo "error: nobody is logged in on the Mac's desktop. Log in there (or over" >&2
		echo "       screen sharing), or turn on automatic login, and run again." >&2
		exit 1; }
	label="lesynth-fourier.sign.$$"
	: > sign.log
	cat > job.plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
	<key>Label</key><string>$label</string>
	<key>ProgramArguments</key><array>
		<string>/bin/bash</string><string>$DIR/sign.sh</string><string>--in-session</string>
	</array>
	<key>RunAtLoad</key><true/>
	<key>StandardOutPath</key><string>$DIR/sign.log</string>
	<key>StandardErrorPath</key><string>$DIR/sign.log</string>
</dict></plist>
EOF
	launchctl bootstrap "gui/$uid" job.plist
	tail -n +1 -f sign.log &
	tailer=$!
	disown $tailer
	trap 'kill $tailer 2>/dev/null; launchctl bootout "gui/$uid/$label" 2>/dev/null || true' EXIT
	while [ ! -f sign.status ]; do sleep 1; done
	sleep 1
	exit "$(cat sign.status)"
fi

trap 'echo $? > sign.status' EXIT
. ./sign.conf   # SIGN_IDENTITY NOTARY_KEY_ID NOTARY_ISSUER

security find-identity -v -p codesigning | grep -qF -- "$SIGN_IDENTITY" || {
	echo "error: no signing identity \"$SIGN_IDENTITY\" in the login keychain:"
	security find-identity -v -p codesigning
	exit 1; }

bundles=()
for b in stage/*/*.vst3 stage/*/*.clap; do [ -d "$b" ] && bundles+=("$b"); done
[ ${#bundles[@]} -gt 0 ] || { echo "error: no .vst3/.clap bundles under stage/"; exit 1; }

echo "── signing ─────────────────────────────────────────────────────────"
echo "(the first run may wait on a key-access dialog on the Mac's screen:"
echo " enter the login password there and click Always Allow)"
for b in "${bundles[@]}"; do
	codesign --force --timestamp --options runtime -s "$SIGN_IDENTITY" "$b"
	codesign --verify --strict --deep --verbose=2 "$b"
done

echo "── notarising (takes a few minutes) ────────────────────────────────"
notary=(--key AuthKey.p8 --key-id "$NOTARY_KEY_ID")
[ -n "$NOTARY_ISSUER" ] && notary+=(--issuer "$NOTARY_ISSUER")
mkdir submit
for b in "${bundles[@]}"; do ditto "$b" "submit/$(basename "$(dirname "$b")")/$(basename "$b")"; done
ditto -c -k --keepParent submit submit.zip
out=$(xcrun notarytool submit submit.zip "${notary[@]}" --wait --output-format json) || true
id=$(plutil -extract id raw -o - - <<<"$out" 2>/dev/null) || { echo "$out"; exit 1; }
status=$(plutil -extract status raw -o - - <<<"$out")
echo "submission $id: $status"
if [ "$status" != Accepted ]; then
	xcrun notarytool log "$id" "${notary[@]}" || true
	exit 1
fi
rm -f AuthKey.p8

echo "── stapling ────────────────────────────────────────────────────────"
for b in "${bundles[@]}"; do
	xcrun stapler staple "$b"
	xcrun stapler validate "$b"
done

echo "── zipping ─────────────────────────────────────────────────────────"
mkdir out
for d in stage/*/; do
	name=$(basename "$d")
	ditto -c -k --keepParent "stage/$name" "out/$name.zip"
	echo "out/$name.zip"
done
