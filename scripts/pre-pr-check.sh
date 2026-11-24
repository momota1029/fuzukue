#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: %s [--privacy-only] [base-ref (default: upstream/main)]\n' "${0##*/}"
}

privacy_only=false
if [[ ${1:-} == --privacy-only ]]; then
    privacy_only=true
    shift
fi
if [[ ${1:-} == --help ]]; then
    usage
    exit 0
fi
if [[ $# -gt 1 || ${1:-} == -* ]]; then
    usage >&2
    exit 2
fi

repo=$(git rev-parse --show-toplevel)
cd "$repo"
base_ref=${1:-upstream/main}
if ! base=$(git rev-parse --verify --quiet --end-of-options "$base_ref^{commit}"); then
    printf 'Base ref unavailable. Fetch the upstream branch or specify its local ref.\n' >&2
    exit 2
fi
head=$(git rev-parse --verify HEAD)
if ! git merge-base "$base" "$head" >/dev/null; then
    printf 'Base and HEAD need a common ancestor. Fetch more history if this clone is shallow.\n' >&2
    exit 2
fi
state=$(git status --porcelain --untracked-files=normal)
if [[ -n $state ]]; then
    printf 'Use a clean checkout of the commits you intend to push.\n' >&2
    exit 2
fi
commits=$(git rev-list "$base..$head")
if [[ -z $commits ]]; then
    printf 'There are no branch commits to check.\n' >&2
    exit 2
fi

temp_root=$(cd "${TMPDIR:-/tmp}" && pwd -P)
if git -C "$temp_root" rev-parse --git-dir >/dev/null 2>&1; then
    printf 'Set TMPDIR to a writable directory outside any Git repository.\n' >&2
    exit 2
fi
export TMPDIR="$temp_root"
scratch=$(mktemp -d "$TMPDIR/fuzukue-pre-pr.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT

# POSIX ERE forms of the git_integrate_patch sanitizer's recognizable patterns.
# Leave fixture loopback names/addresses and opaque strings for manual review.
home='/(home|Users|var/home|mnt/[a-z]/Users)/|/[r]oot([^[:alnum:]_]|$)|[a-z]:[/\\]+(Users|Documents and Settings)[/\\]+'
email="[[:alnum:]._%+'-]+@([[:alnum:]-]+[.])+[[:alpha:]]{2,}"
host='[[:alnum:]_-]+[.](ts[.]net|local|lan|internal|localhost|localdomain|corp)([^[:alnum:]_-]|$)'
octet='(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])'
ip="(^|[^[:alnum:].])(10[.]$octet|172[.](1[6-9]|2[0-9]|3[01])|192[.]168|169[.]254|100[.](6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7]))[.]$octet[.]$octet([^[:alnum:].]|$)"
ipv6='(^|[^[:alnum:]:])(f[cd][0-9a-f]{2}|fe[89ab][0-9a-f]):[0-9a-f:]+([^[:alnum:]:]|$)'
credential='(^|[^[:alnum:]_])(gh[pousr]_[[:alnum:]_]+|github_pat_[[:alnum:]_]+|glpat-[[:alnum:]_-]+|xox[baprs]-[[:alnum:]-]+|sk-(proj-|ant-)?[[:alnum:]_-]{12,}|hf_[[:alnum:]]{12,}|(AKIA|ASIA)[A-Z0-9]{16}|AIza[[:alnum:]_-]{30,}|tskey-[[:alnum:]_-]+|eyJ[[:alnum:]_-]+[.][[:alnum:]_-]+[.][[:alnum:]_-]*)'
material='(begin|end)[^[:cntrl:]]{0,50}private[ _-]+key|openssh-key[-]v1|ssh-(rsa|ed25519)[[:blank:]]+AAAA'
field='[[:alnum:]_-]*(api[-_]?key|access[-_]?key|private[-_]?key|client[-_]?secret|token|password|passwd|secret|cookie)[[:alnum:]_-]*'
value="[^[:space:]\"'<>,;|)}]+|[\"'][^\"']+[\"']"
assignment="(^|[^[:alnum:]_])$field[\"']?[[:blank:]]*[:=][[:blank:]]*($value)|--$field[[:blank:]]+($value)"
auth="([[:alpha:]][[:alnum:]+.-]*://[^[:space:]/<>\"']+@|((proxy-)?authorization|set-cookie|cookie)[[:blank:]]*:[[:blank:]]*($value)|(^|[^[:alnum:]_])bearer[[:blank:]]+($value))"
pattern="$home|$email|$host|$ip|$ipv6|$credential|$material|$assignment|$auth"

findings=false
while IFS= read -r commit; do
    git show --no-show-signature --format=%B --no-patch "$commit" > "$scratch/scan"
    git -c core.quotePath=false show --no-show-signature --format= --patch --root --diff-merges=first-parent --unified=0 \
        --no-color --no-renames --no-ext-diff --no-textconv "$commit" > "$scratch/diff"

    # Hunk state distinguishes added text starting with plus signs from headers.
    awk '
        /^diff --git / { hunk = 0; paths = $0; next }
        !hunk && /^new file mode / { print paths; next }
        /^@@ / { hunk = 1; next }
        hunk && /^\+/ { print substr($0, 2); next }
        !hunk && /^\+\+\+ / && $0 != "+++ /dev/null" { print substr($0, 5) }
        !hunk && /^Binary files / { print }
    ' "$scratch/diff" >> "$scratch/scan"
    if grep -aiE -- "$pattern" "$scratch/scan" >/dev/null; then
        printf 'Possible private data in commit %s. Inspect its message, filenames, and additions locally.\n' "$commit" >&2
        findings=true
    else
        status=$?
        if [[ $status -ne 1 ]]; then
            exit "$status"
        fi
    fi
done <<< "$commits"

if $findings; then
    exit 1
fi
printf 'Privacy scan passed. Review commit identities, binary files, PR text, and attachments manually.\n'

if ! $privacy_only; then
    unset FUZUKUE_SERVICE FUZUKUE_SELF_UPDATE_READY FUZUKUE_SELF_UPDATE_SOURCE
    cargo test --locked --workspace
    cargo run --locked -q -p fuzukue-cards -- check
fi
