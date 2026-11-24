#!/usr/bin/env bash
# Capture the Linux GPUI client on a private virtual display.
set -euo pipefail

usage() {
    echo "usage: scripts/shot.sh [--workspace <second-project-dir>] [--git | --workshop] <project-dir> <scenario.json> [out-dir]" >&2
    echo "       scripts/shot.sh --url ws://host:port/ws <scenario.json> [out-dir]" >&2
    exit 2
}

[[ $# -ge 2 ]] || usage
project=
second_project=
mode=fixture
git_fixture=false
auto_review=0
review_shot=
workshop_fixture=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --workspace)
            [[ $# -ge 4 && -z $second_project ]] || usage
            second_project=$(realpath -e -- "$2")
            [[ -d $second_project ]] || usage
            shift 2
            ;;
        --git)
            [[ $# -ge 3 && $git_fixture == false && $workshop_fixture == false ]] || usage
            git_fixture=true
            shift
            ;;
        --workshop)
            [[ $# -ge 3 && $workshop_fixture == false && $git_fixture == false ]] || usage
            workshop_fixture=true
            shift
            ;;
        *) break ;;
    esac
done
if [[ $1 == --url ]]; then
    [[ $# -ge 3 && $git_fixture == false && $workshop_fixture == false && -z $second_project ]] || usage
    url=$2
    mode=read-only
    shift 2
else
    [[ $# -ge 2 ]] || usage
    project=$(realpath -e -- "$1")
    [[ -d $project ]] || usage
    shift
fi
[[ $# -ge 1 && $# -le 2 ]] || usage
scenario=$(realpath -e -- "$1")
[[ -f $scenario ]] || usage
stem=${scenario##*/}
temp_root=$(realpath -m -- "${TMPDIR:-/tmp}")
out=$(realpath -m -- "${2:-$temp_root/lab-shots/${stem%.json}}")
for source in "$project" "$second_project"; do
    [[ -n $source ]] || continue
    if [[ $out == "$source" || $out == "${source%/}/"* ]]; then
        echo "Choose an output directory outside the original project." >&2
        exit 2
    fi
    if [[ $temp_root == "$source" || $temp_root == "${source%/}/"* ]]; then
        echo "The project directory must not contain the temporary workspace $temp_root." >&2
        exit 2
    fi
done

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
source "$root/scripts/compat-env.sh"
fuzukue_legacy_env FUZUKUE_HARNESS_LANG FUZUKUE_SHOT_VK_DRIVER
source "$root/scripts/shot-fixture.sh"
[[ $(uname -s) == Linux ]] || { echo "shot.sh requires Linux." >&2; exit 2; }
command -v python3 >/dev/null || { echo "Missing dependency: python3" >&2; exit 2; }
# Check before creating files or building. Linux Xvfb uses an abstract Unix socket.
python3 - "$mode" <<'PYTHON'
import os
import socket
import sys

try:
    if sys.argv[1] == "fixture":
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(b"\0fuzukue-shot-" + os.urandom(8).hex().encode())
        listener.listen()
except PermissionError:
    print("headless capture needs loopback sockets; this environment forbids them", file=sys.stderr)
    sys.exit(77)
PYTHON
for program in cargo cc setsid xvfb-run Xvfb xauth xdpyinfo import; do
    command -v "$program" >/dev/null || { echo "Missing dependency: $program" >&2; exit 2; }
done

# Use a separate temporary home and state. Discard local build caches and make
# sure symlinks in the copy cannot point to the original checkout.
run=$(mktemp -d "$temp_root/fuzukue-shot.XXXXXXXX")
if [[ -n ${TMPDIR:-} ]]; then
    export TMPDIR="$temp_root"
fi
server=
server_group=
cleanup() {
    local result=$?
    if [[ -n $server ]]; then
        kill -TERM -- "$server_group" 2>/dev/null || kill "$server" 2>/dev/null || true
        for _ in {1..20}; do
            kill -0 "$server" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL -- "$server_group" 2>/dev/null || true
        wait "$server" 2>/dev/null || true
        if (( result != 0 )); then cat "$run/server.log" >&2; fi
    fi
    if (( result != 0 )) && [[ -s $run/xvfb.log ]]; then tail -n 20 "$run/xvfb.log" >&2; fi
    rm -rf -- "$run"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$run"/{bin,cache,config,data,fonts,home,link,runtime,state,tmp} "$out"
chmod 700 "$run/runtime"
copy_fixture() {
    local source=$1 destination_root=$2 destination link
    mkdir "$destination_root"
    tar -C "$source" --exclude=.git --exclude=.lake --exclude=target -cf - . | tar -C "$destination_root" -xf -
    # A preserved symlink may refer to the source checkout (even through a chain).
    while IFS= read -r -d '' link; do
        destination=$(realpath -m -- "$link")
        if [[ $destination != "$destination_root" && $destination != "$destination_root/"* ]]; then
            rm -- "$link"
        fi
    done < <(find "$destination_root" -type l -print0)
}

if [[ $mode == fixture ]]; then
    copy_fixture "$project" "$run/home/project"
    if [[ -n $second_project ]]; then
        copy_fixture "$second_project" "$run/home/workspace-other"
    fi
    prepare_shot_git_fixture "$run" "$root" "$scenario" "$git_fixture" "$workshop_fixture"
    cp "$root/tests/shots/"{claude,codex} "$run/bin/"
    cp "$root/tests/shots/fixtures/question-card.json" "$run/bin/question-card-input.json"
    cp "$root/tests/shots/fixtures/"{xml-components,ui-kit}.md "$run/bin/"
    # The change history keeps a shadow git repository in the server state.
    git_program=$(command -v git) || { echo "Missing dependency: git" >&2; exit 2; }
    [[ -e $run/bin/git ]] || ln -s "$git_program" "$run/bin/git"
    ln -s "$(command -v python3)" "$run/bin/python3"
    if python3 -c 'import json,sys; sys.exit(json.load(open(sys.argv[1])).get("auto_review") is not True)' "$scenario"; then
        auto_review=1
        review_shot=1
    fi
    if shot_scenario_uses "$scenario" build_paper; then
        for program in latexmk uplatex dvipdfmx pdflatex pdfinfo kpsewhich; do
            executable=$(command -v "$program") || { echo "Missing PDF build dependency: $program" >&2; exit 2; }
            ln -s "$executable" "$run/bin/$program"
        done
    fi
fi

# Minimal Linux installs may ship the runtime sonames without the development
# packages' unversioned linker names. Supply aliases only for this invocation.
for entry in xcb:1 xkbcommon:0 xkbcommon-x11:0; do
    name=lib${entry%:*}.so
    [[ $(cc -print-file-name="$name") == "$name" ]] || continue
    soname=$name.${entry#*:}
    installed=$(cc -print-file-name="$soname")
    if [[ ! -f $installed ]]; then
        echo "Missing runtime library $soname. See docs/headless.md." >&2
        exit 2
    fi
    ln -s "$(realpath "$installed")" "$run/link/$name"
done

# Restrict font discovery to the same named Linux fonts on each machine.
for font in \
    /usr/share/fonts/opentype/ipaexfont-gothic/ipaexg.ttf \
    /usr/share/fonts/opentype/ipaexfont-mincho/ipaexm.ttf \
    /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf \
    /usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf \
    /usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf; do
    [[ -f $font ]] || { echo "Missing capture font $font. See docs/headless.md." >&2; exit 2; }
    ln -s "$font" "$run/fonts/"
done
cat >"$run/fonts.conf" <<FONTS
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
  <dir>$run/fonts</dir>
  <cachedir>$run/cache/fontconfig</cachedir>
  <alias><family>sans-serif</family><prefer><family>DejaVu Sans</family><family>IPAexGothic</family></prefer></alias>
  <alias><family>serif</family><prefer><family>DejaVu Serif</family><family>IPAexMincho</family></prefer></alias>
  <alias><family>monospace</family><prefer><family>DejaVu Sans Mono</family></prefer></alias>
</fontconfig>
FONTS
cat >"$run/settings.toml" <<'SETTINGS'
ui_font = "IPAexGothic"
reading_font = "IPAexMincho"
code_font = "DejaVu Sans Mono"
SETTINGS

target_directory() {
    local destination
    destination=$(cargo metadata --locked --no-deps --format-version 1 --manifest-path "$1" |
        python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
    destination=$(realpath -m "$destination")
    if [[ -n $project && ( $destination == "$project" || $destination == "$project/"* ) ]] ||
        [[ -n $second_project && ( $destination == "$second_project" || $destination == "$second_project/"* ) ]]; then
        destination=$run/build
    fi
    printf '%s\n' "$destination"
}
client_manifest=$root/crates/fuzukue/Cargo.toml
client_target=$(target_directory "$client_manifest")
RUST_FONTCONFIG_DLOPEN=1 LIBRARY_PATH="$run/link${LIBRARY_PATH:+:$LIBRARY_PATH}" \
    cargo build --locked --manifest-path "$client_manifest" --target-dir "$client_target" --features headless-shot >&2
client=$client_target/debug/fuzukue

if [[ $mode == fixture ]]; then
    server_manifest=$root/Cargo.toml
    server_target=$(target_directory "$server_manifest")
    cargo build --locked --manifest-path "$server_manifest" --target-dir "$server_target" -p secretaire >&2
    server_bin=$server_target/debug/secretaire
    # The server's PATH contains the fixture CLIs and, for build_paper, the selected TeX tools.
    launcher=(setsid)
    # The MCP runner owns the entire process group, including the fixture server.
    if [[ -n ${FUZUKUE_SHOT_CLIENT_LOG:-} ]]; then launcher=(); fi
    "${launcher[@]}" env -i PATH="$run/bin" HOME="$run/home" TMPDIR="$run/tmp" LANG=C.UTF-8 \
        XDG_CONFIG_HOME="$run/config" XDG_STATE_HOME="$run/state" \
        FUZUKUE_HARNESS_LANG="${FUZUKUE_HARNESS_LANG:-en}" FUZUKUE_ADDR=127.0.0.1:0 FUZUKUE_AUTO_REVIEW="$auto_review" FUZUKUE_AUTO_BUILD=0 FUZUKUE_APPROVALS=ask FUZUKUE_CODEX_SHOT=1 FUZUKUE_REVIEW_SHOT="$review_shot" \
        RUST_LOG=secretaire=info NO_COLOR=1 \
        "$server_bin" "$run/home/project" >"$run/server.log" 2>&1 &
    server=$!
    if ((${#launcher[@]})); then server_group=-$server; else server_group=$server; fi
    address=
    for _ in {1..150}; do
        address=$(sed -nE 's/.*listening on (127\.0\.0\.1:[0-9]+).*/\1/p' "$run/server.log")
        [[ -z $address ]] || break
        kill -0 "$server" 2>/dev/null || break
        sleep 0.1
    done
    [[ -n $address ]] || { echo "Fixture server did not start." >&2; exit 1; }
    url=ws://$address/ws
else
    export FUZUKUE_SHOT_TOKEN
fi

# Force Mesa's software renderer when available and schedule Xvfb on its own
# unused display. No user's DISPLAY or Wayland socket is used.
icd=${FUZUKUE_SHOT_VK_DRIVER:-}
if [[ -z $icd ]]; then
    for candidate in /usr/share/vulkan/icd.d/lvp*.json; do
        if [[ -r $candidate ]]; then icd=$candidate; break; fi
    done
fi
[[ -r $icd ]] || { echo "Mesa Lavapipe Vulkan ICD is missing. See docs/headless.md." >&2; exit 2; }
(
    unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET FUZUKUE_SELECT FUZUKUE_AUTOPROMPT FUZUKUE_ISSUE FUZUKUE_PAPER FUZUKUE_READ FUZUKUE_PDF FUZUKUE_THEME
    export HOME="$run/home" TMPDIR="$run/tmp" XDG_RUNTIME_DIR="$run/runtime" XDG_CONFIG_HOME="$run/config"
    export XDG_CACHE_HOME="$run/cache" XDG_DATA_HOME="$run/data" XDG_STATE_HOME="$run/state"
    export FONTCONFIG_FILE="$run/fonts.conf" VK_DRIVER_FILES="$icd" LANG=C.UTF-8 LC_ALL=C.UTF-8
    unset LABO_SELECT LABO_AUTOPROMPT LABO_ISSUE LABO_PAPER LABO_READ LABO_PDF LABO_THEME
    export FUZUKUE_SHOT_XVFB=1 FUZUKUE_SHOT_SETTINGS="$run/settings.toml"
    capture=(xvfb-run -a -e "$run/xvfb.log" -s "-screen 0 3840x2160x24 -dpi 96 -nolisten tcp"
        sh -c 'xdpyinfo >/dev/null || exit 1; exec "$@"' shot-display
        "$client" --headless-shot "$scenario" "$out" "$url" "$mode")
    if [[ -n ${FUZUKUE_SHOT_CLIENT_LOG:-} ]]; then
        "${capture[@]}" >"$FUZUKUE_SHOT_CLIENT_LOG" 2>&1
    else
        "${capture[@]}"
    fi
)
