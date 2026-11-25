#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 || ! $1 =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "Usage: $0 <release-tag> <output-dir>" >&2
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
export MACOSX_DEPLOYMENT_TARGET=15.0
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
    build_info=$(xcrun vtool -show-build "$exe")
    printf '%s\n' "$build_info"
    [[ $(awk '$1 == "minos" { print $2 }' <<< "$build_info") == "$MACOSX_DEPLOYMENT_TARGET" ]]
    archive="$binary-$tag-$platform.tar.gz"
    tar -czf "$stage/$archive" -C "$stage/target/release" "$binary" -C "$root" "${files[@]}"
    mv -f -- "$stage/$archive" "$out/$archive"
done

# Keep the standalone client archive for the existing release installer.
app="$stage/fuzukue.app"
contents="$app/Contents"
resources="$contents/Resources"
mkdir -p "$contents/MacOS" "$resources/bin" "$resources/licenses" "$stage/fuzukue.iconset"
cp "$stage/target/release/fuzukue" "$contents/MacOS/"
cp "$root/install.sh" "$resources/install.sh"
install -m 755 "$root/scripts/release/macos/git" "$resources/bin/git"
for notice in "${notices[@]}" "${client_notices[@]}"; do
    cp "$root/$notice" "$resources/licenses/${notice//\//_}"
done

info="$contents/Info.plist"
version=${tag#v}
version=${version%%-*}
plutil -create xml1 "$info"
plutil -insert CFBundleIdentifier -string com.fuzukue.desktop "$info"
plutil -insert CFBundleName -string fuzukue "$info"
plutil -insert CFBundleDisplayName -string fuzukue "$info"
plutil -insert CFBundleExecutable -string fuzukue "$info"
plutil -insert CFBundleIconFile -string fuzukue.icns "$info"
plutil -insert CFBundlePackageType -string APPL "$info"
plutil -insert CFBundleInfoDictionaryVersion -string 6.0 "$info"
plutil -insert CFBundleShortVersionString -string "$version" "$info"
plutil -insert CFBundleVersion -string "$version" "$info"
plutil -insert LSMinimumSystemVersion -string "$MACOSX_DEPLOYMENT_TARGET" "$info"
plutil -insert NSHighResolutionCapable -bool YES "$info"
plutil -insert FuzukueReleaseTag -string "$tag" "$info"
plutil -insert FuzukueReleaseCommit -string "$commit" "$info"
plutil -lint "$info"

xcrun swift "$root/scripts/release/macos/make-icon.swift" "$stage/icon.png"
for side in 16 32 128 256 512; do
    for scale in 1 2; do
        pixels=$((side * scale))
        suffix=
        if [[ $scale == 2 ]]; then suffix=@2x; fi
        sips -z "$pixels" "$pixels" "$stage/icon.png" \
            --out "$stage/fuzukue.iconset/icon_${side}x${side}$suffix.png" >/dev/null
    done
done
iconutil -c icns -o "$resources/fuzukue.icns" "$stage/fuzukue.iconset"
codesign --force --sign - "$app"
codesign --verify --strict "$app"
archive="fuzukue-app-$tag-$platform.tar.gz"
tar -czf "$stage/$archive" -C "$stage" fuzukue.app
mv -f -- "$stage/$archive" "$out/$archive"
