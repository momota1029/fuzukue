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
python3 -c 'import json, pathlib, sys; manifest = json.loads(pathlib.Path(sys.argv[1]).read_text()); assert manifest["protocol"] == 76, manifest["protocol"]' "$assets/macos-release-manifest.json"
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

# Exercise the packaged resolver with the LaunchAgent's PATH shape and real
# Git. The wrapper is a distinct file so the guard will skip the system shim.
guard="$HOME/Applications/fuzukue.app/Contents/Resources/bin"
git_bin="$tmp/Real Git & Tools"
fixture_git=$(command -v git)
mkdir -p "$guard" "$git_bin"
install -m 755 "$root/scripts/release/macos/git" "$guard/git"
reject 'Git is unavailable on this Mac.' env PATH="$guard" "$guard/git" --version
printf '#!/bin/sh\nexec "$FIXTURE_GIT" "$@"\n' > "$git_bin/git"
chmod 755 "$git_bin/git"
(
    export PATH="$guard:$git_bin" FIXTURE_GIT="$fixture_git" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
    unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
    git -C "$project" init --quiet --template=
    git -C "$project" -c user.name='Fixture Writer' -c user.email=fixture@example.invalid \
        commit --quiet --allow-empty -m 'Initialize fuzukue project'
    [[ $(git -C "$project" log -1 --format=%s) == 'Initialize fuzukue project' ]]
    [[ -z $(git -C "$project" status --porcelain) ]]
)
echo 'Packaged Git resolver fixtures passed.'
