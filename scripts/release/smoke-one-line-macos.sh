#!/usr/bin/env bash
# Use the same assets that would be attached to the release. Python is CI-only.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
assets=$1
tag=$2
work=$(mktemp -d "${TMPDIR:-/tmp}/fuzukue-one-line.XXXXXXXX")
work=$(cd "$work" && pwd -P)
export HOME="$work/Test Home & Notes"
export LANG=en_US.UTF-8
unset FUZUKUE_SERVICE FUZUKUE_SELF_UPDATE_READY FUZUKUE_SELF_UPDATE_SOURCE FUZUKUE_SERVER_CONFIG
for tool in sh bash awk codesign cp curl defaults id launchctl mkdir mktemp mv open osascript \
    plutil rm shasum shlock sleep stat sw_vers sysctl tar touch uname xcode-select; do
    (PATH=/usr/bin:/bin:/usr/sbin:/sbin; command -v "$tool") || exit 1
done
fixture="$work/fixture"
app="$HOME/Applications/fuzukue.app"
base="$HOME/Library/Application Support/fuzukue/desktop-install"
agents="$HOME/Library/LaunchAgents"
server_label=com.fuzukue.desktop.server
update_label=com.fuzukue.desktop.update
domain="gui/$(id -u)"
candidate="$fixture/assets/$tag"
mkdir -p "$HOME/.local/bin" "$fixture/api" "$candidate" "$HOME/no-developer"
cp "$assets/"*.tar.gz "$assets/"macos-release-manifest.json "$assets/"SHA256SUMS-macos "$candidate"
port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')

flags=(--fixture "$fixture" --test-port "$port" --skip-agents)
gui=false
if launchctl print "$domain" >/dev/null 2>&1; then
    gui=true
else
    echo 'No GUI launchd domain. Exercising installation and updates without loading the LaunchAgents.'
    flags+=(--stage-only)
fi
client_pid=
direct_server=
cleanup() {
    result=$?
    trap - EXIT
    for pid in "$client_pid" "$direct_server"; do
        if [[ -n $pid ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
    done
    if ((result != 0)); then
        for log in "$HOME"/Library/Logs/fuzukue/*.log "$work"/direct*.log; do
            if [[ -f $log ]]; then tail -80 "$log"; fi
        done
    fi
    if [[ -d $base ]]; then sh "$root/install.sh" --uninstall "${flags[@]}" || result=1; fi
    rm -rf "$work"
    exit "$result"
}
trap cleanup EXIT

offline_index() {
    python3 - "$fixture" "$@" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

fixture = Path(sys.argv[1])
releases = []
for tag in sys.argv[2:]:
    dist = fixture / "assets" / tag
    manifest = json.loads((dist / "macos-release-manifest.json").read_text())
    # Exercise annotated tag resolution and JSON null values from GitHub.
    annotated = hashlib.sha1(tag.encode()).hexdigest()
    (fixture / "api" / f"ref-{tag}.json").write_text(json.dumps({
        "ref": f"refs/tags/{tag}", "object": {"type": "tag", "sha": annotated},
    }))
    (fixture / "api" / f"tag-{annotated}.json").write_text(json.dumps({
        "sha": annotated, "object": {"type": "commit", "sha": manifest["commit"]},
    }))
    releases.append({
        "tag_name": tag, "draft": False, "prerelease": "-" in tag, "body": None,
        "assets": [{"name": path.name, "label": None} for path in dist.iterdir()],
    })
(fixture / "api" / "releases-1.json").write_text(json.dumps(releases))
PY
}
offline_index "$tag"

# Exercise a CLI child launched by the real server. It models a Mac with no
# selected developer tools and fails if the inherited Git resolver is bypassed.
for backend in claude codex; do
    cat > "$HOME/.local/bin/$backend" <<'SH'
#!/bin/sh
case $1 in auth|login) exit 1 ;; esac
export DEVELOPER_DIR="$HOME/no-developer"
case "$(command -v git)" in
    "$HOME/Applications/fuzukue.app/Contents/Resources/bin/git")
        result=0
        PATH="${PATH%%:*}:/usr/bin:/bin" git --version >/dev/null 2>&1 || result=$?
        printf '%s\n' "$result" > "$HOME/$(basename "$0").guarded" ;;
esac
exit 1
SH
    chmod 755 "$HOME/.local/bin/$backend"
done
# A partially received command must never enter the function body.
sed '$d' "$root/install.sh" | sh -s -- "${flags[@]}"
[[ ! -e $base && ! -e $app ]]
# A complete piped script follows the normal entry point.
cat "$root/install.sh" | sh -s -- "${flags[@]}"
[[ -d $HOME/fuzukue-project && -f $base/secretaire && $(< "$base/release") == "$tag" ]]
[[ -s $app/Contents/Resources/fuzukue.icns && -x $app/Contents/MacOS/fuzukue ]]
codesign --verify --strict "$app"
[[ $(lipo -archs "$app/Contents/MacOS/fuzukue") == "$(uname -m)" ]]
[[ $(plutil -extract FuzukueReleaseTag raw "$app/Contents/Info.plist") == "$tag" ]]
plutil -lint "$agents/$server_label.plist" "$agents/$update_label.plist"
[[ $(plutil -extract StartInterval raw "$agents/$update_label.plist") == 21600 ]]
[[ $(plutil -extract ProgramArguments.1 raw "$agents/$update_label.plist") == "$app/Contents/Resources/install.sh" ]]
plist_path=$(plutil -extract EnvironmentVariables.PATH raw "$agents/$server_label.plist")
guard="$app/Contents/Resources/bin"
[[ $plist_path == "$guard:"* ]]

# Verify a clean Git resolution and that a real executable's status propagates.
status=0
DEVELOPER_DIR="$HOME/no-developer" PATH="$guard:/usr/bin:/bin" "$guard/git" --version || status=$?
[[ $status == 127 ]]
guide=$(DEVELOPER_DIR="$HOME/no-developer" LC_ALL=ja_JP.UTF-8 \
    sh "$root/install.sh" --guide "${flags[@]}" --stage-only)
[[ $guide == *'自分のアカウントでログイン'* ]]
status=0
mkdir -p "$work/real-git"
printf '#!/bin/sh\n[ "$1" = --version ] && exit 41\nexit 42\n' > "$work/real-git/git"
chmod 755 "$work/real-git/git"
PATH="$guard:$work/real-git:/usr/bin:/bin" "$guard/git" --version || status=$?
[[ $status == 41 ]]
settings=$(find "$HOME" -name settings.toml -print)
[[ -n $settings && $(< "$settings") == *external* ]]

health="http://127.0.0.1:$port/self-update/health"
health_pid() { curl --noproxy '*' -fsS --max-time 2 "$health"; }
if "$gui"; then
    first_pid=$(health_pid)
    launchctl print "$domain/$server_label" | awk -v pid="$first_pid" '$1 == "pid" && $3 == pid { found=1 } END { exit !found }'
    launchctl print "$domain/$update_label" >/dev/null
else
    PATH="$plist_path" FUZUKUE_MACOS_PACKAGE=1 FUZUKUE_ADDR="127.0.0.1:$port" \
        "$base/secretaire" "$HOME/fuzukue-project" > "$work/direct-server.log" 2>&1 &
    direct_server=$!
    for _ in {1..30}; do if health_pid >/dev/null 2>&1; then break; fi; sleep 1; done
    [[ $(health_pid) == "$direct_server" ]]
fi
if "$gui"; then
    FUZUKUE_URL="ws://127.0.0.1:$port/ws" "$app/Contents/MacOS/fuzukue" > "$work/direct-client.log" 2>&1 &
    client_pid=$!
    for _ in {1..30}; do
        if [[ -f $HOME/codex.guarded ]]; then break; fi
        kill -0 "$client_pid"
        sleep 1
    done
    [[ $(< "$HOME/codex.guarded") == 127 ]]
    kill "$client_pid"; wait "$client_pid" || true
    client_pid=
else
    kill "$direct_server"; wait "$direct_server" || true
    direct_server=
fi

# An invalid download leaves the installed pair untouched. Re-running repairs a missing icon.
archive=$(plutil -extract FuzukueReleaseTag raw "$app/Contents/Info.plist")
case $(uname -m) in arm64) platform=macos-aarch64 ;; x86_64) platform=macos-x86_64 ;; esac
asset="fuzukue-app-$archive-$platform.tar.gz"
printf 'corruption' >> "$candidate/$asset"
if sh "$root/install.sh" "${flags[@]}" > "$work/rejected.log" 2>&1; then exit 1; fi
grep -F 'could not be verified' "$work/rejected.log"
if "$gui"; then [[ $(health_pid) == "$first_pid" ]]; fi
cp "$assets/$asset" "$candidate/$asset"
rm "$app/Contents/Resources/fuzukue.icns"
printf 'keep\n' > "$HOME/fuzukue-project/keep.txt"
sh "$root/install.sh" "${flags[@]}"
codesign --verify --strict "$app"
[[ $(< "$HOME/fuzukue-project/keep.txt") == keep ]]
if "$gui"; then [[ $(health_pid) != "$first_pid" ]]; fi

# A failed settings write happens after replacing the bundle and starting the
# server. The installer must restore both packages and leave the settings alone.
cp "$settings" "$work/original-settings.toml"
printf '[\n' > "$settings"
if sh "$root/install.sh" "${flags[@]}" > "$work/settings-failure.log" 2>&1; then exit 1; fi
grep -F 'connection settings could not be saved' "$work/settings-failure.log"
[[ $(< "$settings") == '[' && $(< "$base/release") == "$tag" ]]
mv "$work/original-settings.toml" "$settings"
codesign --verify --strict "$app"
if "$gui"; then
    for _ in {1..30}; do if health_pid >/dev/null 2>&1; then break; fi; sleep 1; done
    health_pid >/dev/null
fi

# Repackage the same code under a newer tag to exercise the scheduled update path.
next=$(python3 -c 'import re,sys; a,b,c = map(int,re.match(r"v(\d+)\.(\d+)\.(\d+)",sys.argv[1]).groups()); print(f"v{a}.{b}.{c+1}")' "$tag")
next_dir="$fixture/assets/$next"
mkdir -p "$next_dir" "$work/repack"
tar -xzf "$candidate/$asset" -C "$work/repack"
plutil -replace FuzukueReleaseTag -string "$next" "$work/repack/fuzukue.app/Contents/Info.plist"
codesign --force --sign - "$work/repack/fuzukue.app"
COPYFILE_DISABLE=1 tar -czf "$next_dir/fuzukue-app-$next-$platform.tar.gz" -C "$work/repack" fuzukue.app
cp "$candidate/secretaire-$tag-$platform.tar.gz" "$next_dir/secretaire-$next-$platform.tar.gz"
python3 - "$candidate" "$next_dir" "$tag" "$next" "$platform" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

old, new, tag, new_tag, platform = sys.argv[1:]
old, new = Path(old), Path(new)
name = "macos-release-manifest.json"
manifest = json.loads((old / name).read_text())
manifest.update(version=new_tag[1:], channel="stable", artifacts=[
    entry for entry in manifest["artifacts"]
    if entry["platform"] == platform and entry["kind"] in {"server", "macos-app"}
])
sums = []
for entry in manifest["artifacts"]:
    entry["name"] = entry["name"].replace(tag, new_tag)
    file = new / entry["name"]
    entry.update(size=file.stat().st_size, sha256=hashlib.sha256(file.read_bytes()).hexdigest())
    sums.append((entry["sha256"], file.name))
(new / name).write_text(json.dumps(manifest))
sums.append((hashlib.sha256((new / name).read_bytes()).hexdigest(), name))
(new / "SHA256SUMS-macos").write_text("".join(f"{sha}  {file}\n" for sha, file in sums))
PY
offline_index "$tag" "$next"
sh "$root/install.sh" --no-auto-update "${flags[@]}"
sh "$app/Contents/Resources/install.sh" --update "${flags[@]}"
[[ $(< "$base/release") == "$tag" ]]
sh "$root/install.sh" --resume-auto-update "${flags[@]}"
if "$gui"; then
    idle=false
    for _ in {1..30}; do
        if curl --noproxy '*' -fsS "http://127.0.0.1:$port/self-update/wait?package_idle=true" |
            python3 -c 'import json,sys; assert json.load(sys.stdin)["idle"]'; then
            idle=true
            break
        fi
        sleep 1
    done
    [[ $idle == true ]]
fi
sh "$app/Contents/Resources/install.sh" --update "${flags[@]}"
[[ $(< "$base/release") == "$next" ]]
[[ $(plutil -extract FuzukueReleaseTag raw "$app/Contents/Info.plist") == "$next" ]]
codesign --verify --strict "$app"

sh "$root/install.sh" --uninstall "${flags[@]}"
[[ ! -e $app && ! -e $base/secretaire && ! -e $agents/$server_label.plist && ! -e $agents/$update_label.plist ]]
[[ $(< "$HOME/fuzukue-project/keep.txt") == keep && -f $settings ]]
if "$gui"; then
    ! launchctl print "$domain/$server_label" >/dev/null 2>&1
    ! launchctl print "$domain/$update_label" >/dev/null 2>&1
fi
echo 'The macOS one-line installer passed.'
