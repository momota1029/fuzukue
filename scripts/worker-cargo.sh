#!/usr/bin/env bash
set -euo pipefail

# All workers in linked fuzukue worktrees share the primary checkout's target.
git_dir=$(git rev-parse --path-format=absolute --git-common-dir)
if [[ ${git_dir##*/} != .git ]]; then
    echo "worker-cargo needs a non-bare fuzukue checkout" >&2
    exit 2
fi
repo=${git_dir%/.git}
target=$repo/target/worker-shared
if [[ -L $repo/target || -L $target ]]; then
    echo "worker-cargo needs a local target directory, without symlinks" >&2
    exit 2
fi
mkdir -p -- "$target"
exec 9>"$repo/target/worker-shared.lock"
flock 9

export CARGO_TARGET_DIR=$target
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_RELEASE_DEBUG=0

budget_kib=$((20 * 1024 * 1024))
reserve_kib=$((8 * 1024 * 1024))
free_kib() {
    LC_ALL=C df -Pk -- "$target" | awk 'NR == 2 {print $4}'
}
maintain_cache() {
    local size free
    size=$(du -skx -- "$target")
    free=$(free_kib)
    if (( ${size%%[[:space:]]*} > budget_kib || free < reserve_kib )); then
        echo "Clearing the shared worker Cargo target at its storage limit" >&2
        cargo clean --target-dir "$target"
    fi
}

maintain_cache
if (( $(free_kib) < reserve_kib )); then
    echo "worker-cargo needs 8 GiB free on the target volume" >&2
    exit 1
fi

status=0
cargo "$@" || status=$?
maintain_cache || echo "Could not maintain the shared worker Cargo target" >&2
exit "$status"
