#!/usr/bin/env bash
# Run from a checkout with Python 3.8+ and GNU/Linux utilities.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/fuzukue-install-test.XXXXXXXX")
trap 'rm -rf -- "$tmp"' EXIT
export HOME="$tmp/Test Home"
export XDG_DATA_HOME="$tmp/xdg"
export FUZUKUE_CHANNEL=beta FUZUKUE_VERSION=v0.0.0-dogfood.1
unset FUZUKUE_SERVICE FUZUKUE_SELF_UPDATE_READY FUZUKUE_SELF_UPDATE_SOURCE
server="$root/scripts/install-server.sh"
client="$root/scripts/install-client.sh"
project="$HOME/Project & Notes"
assets="$tmp/dist"
mkdir -p "$HOME" "$project" "$assets" "$tmp/payload"
for binary in secretaire fuzukue; do
    printf '#!/bin/sh\nexit 0\n' > "$tmp/payload/$binary"
    for platform in macos-aarch64 macos-x86_64; do
        tar -czf "$assets/$binary-$FUZUKUE_VERSION-$platform.tar.gz" -C "$tmp/payload" "$binary"
    done
done
python3 "$root/scripts/release/make-manifest.py" "$FUZUKUE_VERSION" example/fuzukue "$assets" --macos --include-intel
(cd "$assets" && sha256sum --strict --check SHA256SUMS-macos)
for platform in macos-aarch64 macos-x86_64; do
    output=$(sh "$server" --fixture "$assets" --fixture-platform "$platform" --dry-run --launchd "$project")
    [[ $output == *"Verified secretaire-$FUZUKUE_VERSION-$platform.tar.gz"* ]]
    [[ $output == *"$HOME/Library/Application Support/fuzukue/install/server/$FUZUKUE_VERSION"* ]]
    [[ $output == *"$HOME/Library/LaunchAgents/com.fuzukue.secretaire.plist for $project on 127.0.0.1:7878."* ]]
    output=$(sh "$client" --fixture "$assets" --fixture-platform "$platform" --dry-run)
    [[ $output == *"Verified fuzukue-$FUZUKUE_VERSION-$platform.tar.gz"* ]]
    [[ $output == *"$HOME/Library/Application Support/fuzukue/install/client/$FUZUKUE_VERSION"* ]]
done

reject() {
    local message=$1 output
    shift
    if output=$("$@" 2>&1); then
        echo "Unexpected success for $*" >&2
        return 1
    fi
    [[ $output == *"$message"* ]] || { printf '%s\n' "$output" >&2; return 1; }
}
reject 'require the beta channel' sh "$server" --fixture "$assets" --fixture-platform macos-aarch64 --channel stable --dry-run
reject 'require the beta channel and a prerelease tag' sh "$server" --fixture "$assets" --fixture-platform macos-aarch64 --version v0.0.0 --dry-run
reject '--fixture-platform requires' sh "$server" --fixture "$assets" --fixture-platform macos-aarch64
reject '--launchd requires a macOS server' sh "$client" --fixture "$assets" --fixture-platform macos-aarch64 --dry-run --launchd "$project"

arm="$assets/secretaire-$FUZUKUE_VERSION-macos-aarch64.tar.gz"
printf 'tamper with the archive\n' >> "$arm"
reject 'SHA-256 mismatch' sh "$server" --fixture "$assets" --fixture-platform macos-aarch64 --dry-run
tar -czf "$arm" -C "$tmp/payload" fuzukue
python3 "$root/scripts/release/make-manifest.py" "$FUZUKUE_VERSION" example/fuzukue "$assets" --macos --include-intel
reject 'Expected one regular secretaire' sh "$server" --fixture "$assets" --fixture-platform macos-aarch64 --dry-run
[[ ! -e $HOME/Library && ! -e $HOME/.local && ! -e $XDG_DATA_HOME ]]
echo 'macOS installer dry-run fixtures passed.'
python3 "$root/scripts/release/test-macos-update.py"
