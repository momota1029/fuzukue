#!/usr/bin/env bash
set -euo pipefail

if [[ ${1:-} != /* || ! -d $1 || ${2:-} != v*-* ]]; then
    echo "Usage: $0 <absolute-asset-directory> <prerelease-tag>" >&2
    exit 2
fi
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
assets=$1
tag=$2
work=$(mktemp -d "${TMPDIR:-/tmp}/fuzukue-mac-smoke.XXXXXXXX")
export HOME="$work/Test Home"
export XDG_STATE_HOME="$work/state"
unset FUZUKUE_SERVICE FUZUKUE_SELF_UPDATE_READY FUZUKUE_SELF_UPDATE_SOURCE FUZUKUE_SERVER_CONFIG
export FUZUKUE_CHANNEL=beta FUZUKUE_VERSION=$tag
project="$HOME/Smoke Project & Notes"
agent="$HOME/Library/LaunchAgents/com.fuzukue.secretaire.plist"
domain="gui/$(id -u)"
service="$domain/com.fuzukue.secretaire"
update_service="$domain/com.fuzukue.release-update"
installer="$root/scripts/install-server.sh"
client_installer="$root/scripts/install-client.sh"
cleanup() {
    status=$?
    trap - EXIT
    if ((status != 0)); then
        launchctl print "$service" || true
        for log in "$HOME/Library/Logs/fuzukue/"*.log; do
            if [[ -f $log ]]; then
                tail -100 "$log"
            fi
        done
    fi
    if [[ -f $agent ]]; then
        sh "$installer" --uninstall || status=1
    fi
    rm -rf -- "$work"
    exit "$status"
}
trap cleanup EXIT

mkdir -p -- "$HOME" "$project" "$work/unavailable-backends"
# An exit-127 fixture shadows any coding CLIs present on the runner image.
for backend in claude codex; do
    printf '#!/bin/sh\nexit 127\n' > "$work/unavailable-backends/$backend"
    chmod 755 "$work/unavailable-backends/$backend"
done
export PATH="$work/unavailable-backends:$PATH"
git init -q "$project"
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
# Exercise scheduler registration without querying GitHub or applying a real release.
sh "$installer" --auto-updates off
sh "$installer" --fixture "$assets" --launchd "$project" --health-port "$port" --skip-agents
[[ $("$HOME/.local/bin/secretaire" --version) == "secretaire $tag" ]]
plutil -lint "$agent" "$HOME/Library/LaunchAgents/com.fuzukue.release-update.plist"
launchctl print "$update_service" >/dev/null
health_url="http://127.0.0.1:$port/self-update/health"
first_pid=$(curl --noproxy '*' --max-time 5 -fsS "$health_url")
[[ $first_pid =~ ^[1-9][0-9]*$ ]]
kill -0 "$first_pid"
# Re-register the same version to exercise stopping and restarting an owned agent.
sh "$installer" --fixture "$assets" --launchd "$project" --health-port "$port" --skip-agents
pid=$(curl --noproxy '*' --max-time 5 -fsS "$health_url")
[[ $pid =~ ^[1-9][0-9]*$ && $pid != "$first_pid" ]]
kill -0 "$pid"
curl --noproxy '*' --max-time 5 -fsS "http://127.0.0.1:$port/self-update/wait?package_idle=true" |
    python3 -c 'import json, sys; status = json.load(sys.stdin); assert status["idle"] is False and status["pid"] == int(sys.argv[1])' "$pid"
sh "$client_installer" --fixture "$assets"
[[ $("$HOME/.local/bin/fuzukue" --version) == "fuzukue $tag" ]]
client="$HOME/Library/Application Support/fuzukue/install/client/$tag/fuzukue"
codesign --verify "$client"
[[ $(lipo -archs "$client") == "$(uname -m)" ]]
sh "$client_installer" --uninstall
sh "$installer" --uninstall
[[ ! -e $agent && ! -e $HOME/.local/bin/secretaire && ! -e $HOME/.local/bin/fuzukue ]]
for _ in {1..30}; do
    if ! kill -0 "$pid" 2>/dev/null; then
        break
    fi
    sleep 1
done
if kill -0 "$pid" 2>/dev/null; then
    echo "The server process did not stop." >&2
    exit 1
fi
for label in "$service" "$update_service"; do
    if launchctl print "$label" >/dev/null 2>&1; then
        echo "The launch agent is still registered: $label" >&2
        exit 1
    fi
done
[[ -d $XDG_STATE_HOME && -d $project/.git ]]
