#!/bin/sh
# Build and install both fuzukue components from this checkout.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)

if [ "$(id -u)" = 0 ]; then
    echo 'Run setup from your regular user account.' >&2
    exit 1
fi

# Probe the active developer directory before calling Apple's Python shim.
if [ "$(uname -s)" = Darwin ] && ! xcode-select -p >/dev/null 2>&1; then
    printf 'macOS %s (%s)\n' "$(sw_vers -productVersion)" "$(uname -m)"
    echo 'Install Apple Command Line Tools with: xcode-select --install'
    for arg do
        case "$arg" in
            --dry-run) echo 'After installation, rerun the preview for the full plan.'; exit 0 ;;
            -h|--help) echo 'Usage: sh scripts/setup-unix.sh [--project DIR] [--dry-run | --build-only] [--skip-agents]'; exit 0 ;;
        esac
    done
    if [ -t 0 ] && [ -t 2 ]; then
        printf 'Open the Apple installer now? [y/N] ' >&2
        if read -r answer; then
            case "$answer" in y|Y|yes|YES|Yes) xcode-select --install ;; esac
        fi
    fi
    echo 'After installation finishes, run setup again.'
    exit 1
fi

if ! python3 -c 'import sys; sys.exit(sys.version_info < (3, 8))' 2>/dev/null; then
    printf '%s %s (%s)\n' "$(uname -s)" "$(uname -r)" "$(uname -m)"
    echo 'Setup needs Python 3.8 or newer.'
    case "$(uname -s)" in
        Darwin) echo 'Install Python from https://www.python.org/downloads/macos/ and reopen Terminal.' ;;
        Linux)
            echo 'Debian/Ubuntu: sudo apt-get install python3'
            echo 'Fedora: sudo dnf install python3' ;;
    esac
    exit 1
fi
exec python3 "$root/scripts/setup-unix.py" "$@"
