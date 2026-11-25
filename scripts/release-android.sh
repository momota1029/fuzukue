#!/usr/bin/env bash
set -euo pipefail

usage() {
    printf 'Usage: FUZUKUE_ANDROID_RELEASE_REPO=OWNER/REPO %s [--dry-run] [release-notes.md]\n' "$0"
    printf '       %s [--dry-run] --server <stable|beta|dev> <project> [release-notes.md]\n' "$0"
}

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

dry_run=false
case ${1:-} in
    --help|-h) usage; exit 0 ;;
    --dry-run) dry_run=true; shift ;;
esac
channel=
if [[ ${1:-} == --server ]]; then
    [[ $# -ge 3 ]] || fail 'Specify an Android channel and the running server project.'
    channel=$2
    case $channel in stable|beta|dev) ;; *) fail 'Unknown Android channel.' ;; esac
    project=$(realpath -- "$3")
    shift 3
fi
if [[ $# -gt 1 || ($dry_run == false && -z $channel && $# -ne 1) ]]; then
    usage >&2
    exit 2
fi
if [[ $# -eq 1 && (! -f $1 || ! -r $1) ]]; then
    fail "Cannot read release notes: $1"
fi

if [[ -z $channel ]]; then
    release_repo=${FUZUKUE_ANDROID_RELEASE_REPO:-}
    [[ $release_repo =~ ^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9_.-]+$ && ${release_repo#*/} != . && ${release_repo#*/} != .. ]] \
        || fail 'Set FUZUKUE_ANDROID_RELEASE_REPO to the intended GitHub OWNER/REPO.'
fi

default_notes=release-notes.md
if [[ -n $channel ]]; then default_notes=/dev/null; fi
notes=$(realpath -m -- "${1:-$default_notes}")
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
"$root/scripts/check-android-protocol.sh"
"$root/scripts/check-android-notices.sh"
android="$root/android"
version=$(sed -nE 's/^versionName[[:space:]]*=[[:space:]]*([^[:space:]]+)[[:space:]]*$/\1/p' "$android/version.properties")
version_code=$(sed -nE 's/^versionCode[[:space:]]*=[[:space:]]*([^[:space:]]+)[[:space:]]*$/\1/p' "$android/version.properties")
minimum_protocol=$(sed -nE 's/^internal const val MIN_PROTOCOL_VERSION = ([0-9]+)$/\1/p' \
    "$android/app/src/main/java/dev/labgui/remote/Protocol.kt")
maximum_protocol=$(sed -nE 's/^const val PROTOCOL_VERSION = ([0-9]+)$/\1/p' \
    "$android/app/src/main/java/dev/labgui/remote/Protocol.kt")
[[ $version =~ ^[0-9][A-Za-z0-9._+\-]*$ && $version_code =~ ^[1-9][0-9]*$ && $minimum_protocol =~ ^[1-9][0-9]*$ && $maximum_protocol =~ ^[1-9][0-9]*$ ]] \
    || fail 'Invalid Android release version or protocol range.'

output="$android/build/releases/$version"
apk="fuzukue-android-$version.apk"
release_tag=android-beta
release_title="fuzukue for Android v$version"
release_command() {
    printf 'First beta (the tag will be created on the releases repository default branch):\n'
    printf 'gh release create'
    printf ' %q' "$release_tag" "$output/$apk" "$output/$apk.sha256" "$output/android-latest.json" \
        --repo "$release_repo" --prerelease --latest=false --title "$release_title" --notes-file "$notes"
    printf '\n\nSubsequent betas (upload the APK and checksum before replacing the feed):\n'
    printf 'gh release upload'
    printf ' %q' "$release_tag" "$output/$apk" "$output/$apk.sha256" --clobber --repo "$release_repo"
    printf ' &&\n'
    printf 'gh release upload'
    printf ' %q' "$release_tag" "$output/android-latest.json" --clobber --repo "$release_repo"
    printf ' &&\n'
    printf 'gh release edit'
    printf ' %q' "$release_tag" --repo "$release_repo" --prerelease --latest=false \
        --title "$release_title" --notes-file "$notes"
    printf '\n'
}

if $dry_run; then
    printf 'Would run :app:assembleRelease in %s and verify its signature with apksigner.\n' "$android"
    if [[ -n $channel ]]; then
        printf 'Would register versionCode %s (protocol %s–%s) for %s in %s.\n' \
            "$version_code" "$minimum_protocol" "$maximum_protocol" "$channel" "$project"
        printf 'Feed: /android/%s/android-latest.json\nFirst install: /android/%s/download.apk\n' "$channel" "$channel"
    else
        printf 'Would write %s, %s and android-latest.json to %s (versionCode %s, minimum protocol %s).\n' \
            "$apk" "$apk.sha256" "$output" "$version_code" "$minimum_protocol"
        release_command
    fi
    exit 0
fi

apksigner=$(command -v apksigner || true)
if [[ -z $apksigner ]]; then
    sdk=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}
    if [[ -z $sdk && -f $android/local.properties ]]; then
        sdk=$(sed -n 's/^sdk\.dir=//p' "$android/local.properties")
    fi
    if [[ -n $sdk ]]; then
        while IFS= read -r candidate; do
            if [[ -x $candidate ]]; then
                apksigner=$candidate
                break
            fi
        done < <(printf '%s\n' "$sdk"/build-tools/*/apksigner | sort -Vr)
    fi
fi
[[ -n $apksigner ]] || fail 'Install Android SDK Build Tools or put apksigner on PATH.'
if [[ -n $channel ]]; then
    aapt="$(dirname -- "$apksigner")/aapt"
    if [[ ! -x $aapt ]]; then
        aapt=$(command -v aapt || true)
    fi
    [[ -n $aapt ]] || fail 'Install Android SDK Build Tools or put aapt on PATH.'
    secretaire=${FUZUKUE_SECRETAIRE:-secretaire}
    command -v "$secretaire" >/dev/null || fail 'Set FUZUKUE_SECRETAIRE to the new secretaire binary.'
fi

mkdir -p -- "$android/build/releases"
exec {release_lock}>"$android/build/releases/.release.lock"
flock "$release_lock"
(cd -- "$android" && ./gradlew :app:assembleRelease) >&2

source_apk="$android/app/build/outputs/apk/release"
staging=$(mktemp -d "$android/build/releases/.staging.XXXXXXXX")
trap 'rm -rf -- "$staging"' EXIT
python3 - "$source_apk" "$staging" "$apk" "$version" "$version_code" "$minimum_protocol" "$maximum_protocol" "$notes" <<'PY'
import hashlib
import json
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path

source, staging, apk, version, version_code, minimum_protocol, maximum_protocol, notes = sys.argv[1:]
source, staging = Path(source), Path(staging)
metadata = json.loads((source / "output-metadata.json").read_text(encoding="utf-8"))
application_id = metadata.get("applicationId")
if not isinstance(application_id, str) or not application_id:
    sys.exit("Release APK metadata is missing its application ID.")
elements = metadata["elements"]
if len(elements) != 1 or elements[0]["outputFile"] != "app-release.apk":
    sys.exit("Expected a signed release APK. Check the Android signing configuration.")
if elements[0]["versionName"] != version or elements[0]["versionCode"] != int(version_code):
    sys.exit("Release APK version does not match android/version.properties.")

shutil.copyfile(source / "app-release.apk", staging / apk)
with (staging / apk).open("rb") as stream:
    digest = hashlib.sha256()
    for block in iter(lambda: stream.read(1024 * 1024), b""):
        digest.update(block)
    sha256 = digest.hexdigest()
with Path(notes).open(encoding="utf-8") as stream:
    first_line = stream.readline().rstrip("\n")
manifest = json.dumps({
    "version": version,
    "versionCode": int(version_code),
    "package": application_id,
    "sha256": sha256,
    "apk": apk,
    "minimum_protocol": int(minimum_protocol),
    "maximum_protocol": int(maximum_protocol),
    "release_notes_first_line": first_line,
    "built_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
}, ensure_ascii=False, indent=2) + "\n"
if len(manifest.encode("utf-8")) > 32_768:
    sys.exit("Keep the release notes' first line short enough for the update manifest.")
(staging / f"{apk}.sha256").write_text(f"{sha256}  {apk}\n", encoding="ascii")
(staging / "android-latest.json").write_text(manifest, encoding="utf-8")
PY
if [[ -n $channel ]]; then
    "$secretaire" publish-android "$channel" "$project" "$staging/android-latest.json" "$staging/$apk" "$apksigner" "$aapt"
    exit 0
fi
"$apksigner" verify "$staging/$apk" >&2
mkdir -p -- "$output"
mv -f -- "$staging/$apk" "$staging/$apk.sha256" "$staging/android-latest.json" "$output/"
release_command
