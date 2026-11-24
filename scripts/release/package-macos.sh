#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 || ! $1 =~ ^v[0-9]+\.[0-9]+\.[0-9]+-[0-9A-Za-z.-]+$ ]]; then
    echo "Usage: $0 <prerelease-tag> <output-dir>" >&2
    exit 2
fi
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=macos-aarch64 ;;
    Darwin-x86_64) platform=macos-x86_64 ;;
    *) echo 'A native macOS runner is required.' >&2; exit 2 ;;
esac

tag=$1
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
commit=$(git -C "$root" rev-parse --verify 'HEAD^{commit}')
export FUZUKUE_SERVER_BUILD_ID=$commit FUZUKUE_CLIENT_BUILD_ID=$commit
export COPYFILE_DISABLE=1
mkdir -p -- "$2"
out=$(cd -- "$2" && pwd -P)
stage=$(mktemp -d "${TMPDIR:-/tmp}/fuzukue-package.XXXXXXXX")
trap 'rm -rf -- "$stage"' EXIT

cargo build --locked --release --manifest-path "$root/Cargo.toml" --target-dir "$stage/target" -p secretaire
cargo build --locked --release --manifest-path "$root/crates/fuzukue/Cargo.toml" --target-dir "$stage/target"

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
    exe="$stage/target/release/$binary"
    [[ $(lipo -archs "$exe") == "$(uname -m)" ]]
    # Include a valid ad-hoc code signature on each Mach-O for local execution.
    codesign --force --sign - "$exe"
    codesign --verify "$exe"
    otool -L "$exe"
    xcrun vtool -show-build "$exe"
    archive="$binary-$tag-$platform.tar.gz"
    tar -czf "$stage/$archive" -C "$stage/target/release" "$binary" -C "$root" "${files[@]}"
    mv -f -- "$stage/$archive" "$out/$archive"
done
