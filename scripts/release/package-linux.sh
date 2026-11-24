#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 || ! $1 =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "Usage: $0 <version-tag> <output-dir>" >&2
    exit 2
fi
if [[ $(uname -s)-$(uname -m) != Linux-x86_64 ]]; then
    echo 'Linux x86_64 is required.' >&2
    exit 2
fi

tag=$1
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
commit=$(git -C "$root" rev-parse --verify 'HEAD^{commit}')
export FUZUKUE_SERVER_BUILD_ID=$commit FUZUKUE_CLIENT_BUILD_ID=$commit
out=$(realpath -m -- "$2")
mkdir -p -- "$out"
stage=$(mktemp -d "$out/.linux.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT

cargo build --locked --release --manifest-path "$root/Cargo.toml" --target-dir "$stage/target" -p secretaire
RUST_FONTCONFIG_DLOPEN=1 cargo build --locked --release \
    --manifest-path "$root/crates/fuzukue/Cargo.toml" --target-dir "$stage/target"

notices=(LICENSE-APACHE LICENSE-MIT NOTICE THIRD_PARTY_NOTICES)
client_notices=(
    vendor/gpui-base/LICENSE-APACHE vendor/gpui-pre/LICENSE-APACHE vendor/gpui-pdf/LICENSE
    crates/fuzukue/assets/icons/LICENSE crates/fuzukue/assets/icons/LICENSE-FEATHER
)
for binary in secretaire fuzukue; do
    files=("${notices[@]}")
    if [[ $binary == fuzukue ]]; then
        files+=("${client_notices[@]}")
    fi
    archive="$binary-$tag-linux-x86_64.tar.gz"
    tar -czf "$stage/$archive" -C "$stage/target/release" "$binary" -C "$root" "${files[@]}"
    mv -f -- "$stage/$archive" "$out/$archive"
done
