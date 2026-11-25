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
if [[ -z $channel ]]; then
    apk="fuzukue-android-${version//+/-}-$version_code.apk"
fi
release_tag=android-beta
release_title="fuzukue for Android v$version"
github_plan() {
    printf 'GitHub repository: %s\n' "$release_repo"
    printf 'Would look up %s and create a draft prerelease if missing (tag from the default branch).\n' "$release_tag"
    printf 'Would upload %s, then %s, keeping previous versions and reusing identical assets.\n' "$apk" "$apk.sha256"
    printf 'Would back up any current feed and write android-latest.json last if it changed (attempt restoration if replacement fails).\n'
    printf 'Would publish or update %s as a prerelease with title "%s" and notes from "%s".\n' "$release_tag" "$release_title" "$notes"
    printf 'Feed: https://github.com/%s/releases/download/%s/android-latest.json\n' "$release_repo" "$release_tag"
}

gh_release() {
    gh release "$@" --repo "$release_repo"
}

github_has_asset() {
    [[ $'\n'$asset_names$'\n' == *$'\n'"$1"$'\n'* ]]
}

github_upload_version_asset() {
    local name=$1
    if github_has_asset "$name"; then
        gh_release download "$release_tag" --pattern "$name" --dir "$previous"
        cmp -s -- "$previous/$name" "$output/$name" ||
            fail "GitHub asset $name already has different contents. Use a new Android version."
    else
        gh_release upload "$release_tag" "$output/$name"
    fi
}

github_publish() {
    local asset_names replace_feed previous="$staging/previous" feed=android-latest.json
    if ! asset_names=$(gh_release view "$release_tag" --json assets --jq '.assets[].name'); then
        gh_release create "$release_tag" --draft --prerelease --latest=false \
            --title "$release_title" --notes-file "$notes"
        asset_names=
    fi
    mkdir -- "$previous"
    github_upload_version_asset "$apk"
    github_upload_version_asset "$apk.sha256"

    if github_has_asset "$feed"; then
        gh_release download "$release_tag" --pattern "$feed" --dir "$previous"
        replace_feed=$(python3 -c '
import json
import sys
from pathlib import Path

old, new = (json.loads(Path(path).read_text(encoding="utf-8")) for path in sys.argv[1:])
if old["versionCode"] > new["versionCode"] or (
    old["versionCode"] == new["versionCode"] and
    (old["apk"], old["sha256"].lower()) != (new["apk"], new["sha256"].lower())
):
    sys.exit("The GitHub feed already uses this versionCode or a newer one.")
for manifest in (old, new):
    manifest.pop("built_at", None)
print("yes" if old != new else "no")
' "$previous/$feed" "$output/$feed")

        if [[ $replace_feed == yes ]] && ! gh_release upload "$release_tag" "$output/$feed" --clobber; then
            if ! gh_release upload "$release_tag" "$previous/$feed" --clobber; then
                cp -- "$previous/$feed" "$output/previous-$feed"
                fail "Could not restore the GitHub feed. Its backup is at $output/previous-$feed."
            fi
            fail 'Could not update the GitHub feed. The previous feed was restored.'
        fi
    else
        gh_release upload "$release_tag" "$output/$feed"
    fi
    gh_release edit "$release_tag" --draft=false --prerelease --latest=false \
        --title "$release_title" --notes-file "$notes"
}

if $dry_run; then
    printf 'Would run :app:assembleRelease in %s and verify its signature with apksigner.\n' "$android"
    if [[ -n $channel ]]; then
        printf 'Would register versionCode %s (protocol %s–%s) for %s in %s.\n' \
            "$version_code" "$minimum_protocol" "$maximum_protocol" "$channel" "$project"
        printf 'Feed: /android/%s/android-latest.json\nFirst install: /android/%s/download.apk\n' "$channel" "$channel"
    else
        printf 'Would write %s, %s and android-latest.json to %s (versionCode %s, protocol %s–%s).\n' \
            "$apk" "$apk.sha256" "$output" "$version_code" "$minimum_protocol" "$maximum_protocol"
        github_plan
    fi
    exit 0
fi

if [[ -z $channel ]]; then
    command -v gh >/dev/null || fail 'Install and authenticate the GitHub CLI to publish an Android beta.'
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
github_publish
