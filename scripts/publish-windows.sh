#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
source "$root/scripts/compat-env.sh"
fuzukue_legacy_env FUZUKUE_PROJECT_DIR FUZUKUE_SERVER_BINARY
phase=both
case ${1:-} in
    build|register) phase=$1; shift ;;
esac
project=${1:-${FUZUKUE_PROJECT_DIR:-$PWD}}
server=${FUZUKUE_SERVER_BINARY:-"$root/target/release/secretaire"}
commit=$(git -C "$root" rev-parse HEAD)

publish() {
    FUZUKUE_CLIENT_BUILD_ID="$commit" LABO_CLIENT_BUILD_ID="$commit" "$server" publish-client windows-x86_64 "$1" "$project"
}

# The self-update checkout carries its own copy across the server swap. A later
# build in the shared Windows target cannot invalidate this registration.
if [[ -n ${FUZUKUE_WINDOWS_BUILD_STATE:-} ]] &&
   [[ $phase == register || ($phase == both && ${FUZUKUE_PREBUILT:-} == 1) ]]; then
    build_state=$FUZUKUE_WINDOWS_BUILD_STATE
    if [[ ! -f "$build_state" || $(<"$build_state") != "$commit" || ! -f "$build_state.exe" ]]; then
        printf 'No staged Windows build for commit %s (%s)\n' "$commit" "$build_state" >&2
        exit 1
    fi
    publish "$build_state.exe"
    exit
fi

# A systemd service can have Windows interop without the WSL session variables.
if [[ -z ${WSL_DISTRO_NAME:-} ]] && grep -qi microsoft /proc/version && windows_root=$(wslpath -w /); then
    case $windows_root in
        '\\wsl.localhost\'*) distro=${windows_root#'\\wsl.localhost\'} ;;
        '\\wsl$\'*) distro=${windows_root#'\\wsl$\'} ;;
        *) distro= ;;
    esac
    distro=${distro%\\}
    if [[ -n $distro && $distro != *'\'* ]]; then
        export WSL_DISTRO_NAME=$distro
    fi
fi
: "${WSL_DISTRO_NAME:?Run this script from WSL}"
user=$(powershell.exe -NoProfile -Command '$env:USERNAME' | tr -d '\r')
target="C:\\Users\\$user\\fuzukue-target"
# Build from a Windows-local mirror: reading the sources and vendored crates through
# \\wsl.localhost makes cargo's fingerprinting and rustc's file reads slow.
mirror_win="C:\\Users\\$user\\fuzukue-src"
mirror=$(wslpath -u "$mirror_win")
exe=$(wslpath -u "$target\\release\\fuzukue.exe")
mirror_state="$mirror.build"
build_state=${FUZUKUE_WINDOWS_BUILD_STATE:-$mirror_state}

# The mirror, target executable and default receipt are shared by all checkouts.
exec {build_lock}>>"$mirror.lock"
flock "$build_lock"

build() (
    rm -f -- "$mirror_state"
    if [[ "$build_state" != "$mirror_state" ]]; then
        rm -f -- "$build_state" "$build_state.exe"
    fi
    mkdir -p "$mirror"
    rsync -a --delete --exclude=.git --exclude=target/ "$root/" "$mirror/"
    if [[ $(git -C "$root" rev-parse HEAD) != "$commit" ]]; then
        printf 'Windows build checkout changed while copying sources\n' >&2
        exit 1
    fi
    local source="$mirror_win\\crates\\fuzukue"
    # PowerShell single-quoted strings escape apostrophes by doubling them.
    local target_ps=${target//\'/\'\'} source_ps=${source//\'/\'\'}
    if [[ -e "$exe" ]]; then
        # Windows can rename a running executable but cannot replace it.
        local old="$exe.old" backup
        for backup in "$exe".old*; do
            rm -f -- "$backup" || true
        done
        if [[ -e "$old" ]]; then
            if backup=$(mktemp -- "$old.XXXXXXXX"); then
                mv -fT -- "$exe" "$backup" || rm -f -- "$backup" || true
            fi
        else
            mv -fT -- "$exe" "$old" || true
        fi
    fi
    powershell.exe -NoProfile -Command "\$ErrorActionPreference='Stop'; \$env:CARGO_TARGET_DIR='$target_ps'; \$env:FUZUKUE_CLIENT_BUILD_ID='$commit'; \$env:CARGO_PROFILE_RELEASE_INCREMENTAL='true'; Set-Location -LiteralPath '$source_ps'; cargo build --release; exit \$LASTEXITCODE" >&2
    test -f "$exe"

    local receipt requested_receipt= staged=
    receipt=$(mktemp -- "$mirror_state.XXXXXXXX")
    trap 'rm -f -- "$receipt" "$requested_receipt" "$staged"' EXIT
    # The unique receipt ID also distinguishes repeated builds of the same commit.
    printf '%s\n%s\n%s\n' "$exe" "$commit" "${receipt##*/}" > "$receipt"
    if [[ "$build_state" != "$mirror_state" ]]; then
        staged=$(mktemp -- "$build_state.exe.XXXXXXXX")
        cp -p -- "$exe" "$staged"
        requested_receipt=$(mktemp -- "$build_state.XXXXXXXX")
        printf '%s\n' "$commit" > "$requested_receipt"
        sync -- "$staged" "$requested_receipt"
        mv -fT -- "$staged" "$build_state.exe"
        mv -fT -- "$requested_receipt" "$build_state"
        sync -- "$(dirname -- "$build_state")"
    fi
    mv -fT -- "$receipt" "$mirror_state"
)

register() {
    local -a receipt=()
    if ! mapfile -t receipt < "$mirror_state" ||
       [[ ${receipt[0]:-} != "$exe" || ${receipt[1]:-} != "$commit" || ! -f "$exe" ]]; then
        printf 'No current Windows build for commit %s (%s)\n' "$commit" "$mirror_state" >&2
        return 1
    fi
    publish "$exe"
}

case $phase in
    build) build; printf '%s\n' "$exe" ;;
    register) register ;;
    both)
        if [[ ${FUZUKUE_PREBUILT:-} != 1 ]]; then
            build
        fi
        register
        ;;
esac
