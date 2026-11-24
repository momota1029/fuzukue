#!/usr/bin/env bash
set -eu
set -o pipefail

default_prefix=/usr/local/share/fuzukue-recipe-android
prefix=$default_prefix
dry_run=false
assume_yes=false
run_self_test=false
entries=(jdk sdk wrapper gradle-deps robolectric STAGED.txt)

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

while (($#)); do
    case $1 in
        --prefix)
            (($# >= 2)) && [[ $2 == /* ]] || fail '--prefix requires an absolute directory path.'
            prefix=$2
            shift
            ;;
        --dry-run) dry_run=true ;;
        --yes) assume_yes=true ;;
        --self-test) run_self_test=true ;;
        -h|--help)
            cat <<EOF
Usage: $0 [--dry-run] [--yes] [--prefix /absolute/path]
       $0 --self-test
Default prefix: $default_prefix
Inputs: JAVA_HOME (default: \$HOME/opt/jdk-21.0.13.1+2),
        ANDROID_HOME (default: \$HOME/Android/Sdk), TMPDIR (default: /tmp).
Run as the operator from a reviewed Linux checkout. Dry run prints the plan
and recipe without probing isolation, writing files, or invoking Gradle.
Self-test exercises publication and rollback under TMPDIR without Gradle or network.
EOF
            exit 0
            ;;
        *) fail "Unknown option '$1'. See --help." ;;
    esac
    shift
done

step() {
    step_name=$1
    shift
    printf '\n%s\n' "$step_name"
    printf '  %s\n' "$@"
    if ! $dry_run && ! $assume_yes; then
        printf 'Continue? [y/N] '
        IFS= read -r answer || fail 'No confirmation received. Use --yes for unattended execution.'
        [[ $answer == y || $answer == Y || $answer == yes ]] || fail 'Stopped at operator request.'
    fi
}

sudo_hint() {
    printf '  sudo install -d -o "$USER" -m 0755 -- %q\n' "$prefix"
}

show_probe() {
    printf '  '
    printf '%q ' "${probe[@]}"
    printf '\n'
}

service_note() {
    local account
    if [[ $(systemctl --user show -p LoadState --value fuzukue.service 2>/dev/null) == loaded ]]; then
        printf '  Found systemd user unit fuzukue.service for %s.\n' "$(id -un)"
    elif [[ $(systemctl show -p LoadState --value fuzukue.service 2>/dev/null) == loaded ]]; then
        account=$(systemctl show -p User --value fuzukue.service 2>/dev/null) || account='(unknown)'
        printf '  Found system unit fuzukue.service with User=%s.\n' "${account:-root (systemd default)}"
    else
        printf '  No local fuzukue.service unit detected. For a manual launch, use the account that starts secretaire.\n'
    fi
    printf '  The same probe must pass for the secretaire service account. The service also needs read/traverse access to the prefix.\n'
}

check_links() {
    local root=$1 source=$2 link target
    target=$(realpath -e -- "$source") || fail "Cannot resolve $source. Reinstall the toolchain component."
    [[ $target == "$root" || $target == "$root/"* ]] || fail "Source escapes $root: $source. Use a self-contained toolchain."
    while IFS= read -r -d '' link; do
        target=$(realpath -e -- "$link") || fail "Broken toolchain link: $link. Reinstall the toolchain component."
        [[ $target == "$root/"* ]] || fail "Link escapes $root: $link. Use a self-contained toolchain."
    done < <(find "$target" -type l -print0)
}

check_sdk_licenses() {
    python3 - "$sdk" "${sdk_packages[@]}" <<'PY'
import hashlib
from pathlib import Path
import re
import sys
import xml.etree.ElementTree as xml

sdk = Path(sys.argv[1])
for component in sys.argv[2:]:
    try:
        package = xml.parse(sdk / component / "package.xml").getroot()
        for element in package.iter():
            element.tag = element.tag.rsplit("}", 1)[-1]
        license_id = package.find("./localPackage/uses-license").attrib["ref"]
        if not re.fullmatch(r"[a-zA-Z0-9_-]+", license_id):
            raise ValueError("invalid license identifier")
        license_text = next(item.text for item in package.findall("license")
                            if item.get("id") == license_id and item.text)
        digest = hashlib.sha1(license_text.encode("utf-8")).hexdigest()
        accepted = (sdk / "licenses" / license_id).read_text().splitlines()
        if digest not in accepted:
            raise ValueError(f"license {license_id} is not accepted")
    except (AttributeError, KeyError, OSError, StopIteration, ValueError, xml.ParseError) as error:
        sys.exit(f"{component}: {error}. Check the SDK Manager installation and review its licenses.")
PY
}

seed=
candidate=
backup=
publishing=false

remove_tree() {
    chmod -R u+rwX -- "$1" && rm -rf --one-file-system -- "$1"
}

clean_leftover_candidates() {
    local directory all_candidates=true
    local -a leftovers=()
    for directory in "$prefix"/.candidate.*; do
        if [[ -d $directory && ! -L $directory ]]; then
            leftovers+=("$directory")
        elif [[ -e $directory || -L $directory ]]; then
            all_candidates=false
        fi
    done
    ((${#leftovers[@]})) || return 0
    if $all_candidates; then
        printf '  To remove leftover candidates manually while staging is idle, run:\n'
        printf '  chmod -R u+w %q/.candidate.* && rm -rf %q/.candidate.*\n' "$prefix" "$prefix"
    fi
    step 'Remove leftover candidate directories' "${leftovers[@]}"
    for directory in "${leftovers[@]}"; do
        remove_tree "$directory" || fail "Could not remove leftover candidate $directory."
    done
}

move_stage_entry() {
    local source=$1 destination=$2
    # A cross-parent directory rename updates its '..' entry and needs write permission.
    if [[ -d $source && ! -w $source ]]; then
        chmod u+w -- "$source" || return 1
        if mv -T -- "$source" "$destination"; then
            chmod u-w -- "$destination"
        else
            chmod u-w -- "$source"
            return 1
        fi
    else
        mv -T -- "$source" "$destination"
    fi
}

publish_stage() {
    local entry
    local -a staged_paths=()
    # Check every entry before moving anything so rollback can identify completed renames.
    for entry in "${entries[@]}"; do
        [[ -e $candidate/$entry ]] || fail "Missing candidate entry: $candidate/$entry"
        if [[ -d $candidate/$entry ]]; then
            chmod u+w -- "$candidate/$entry"
        fi
        staged_paths+=("$prefix/$entry")
    done
    backup=$(mktemp -d "$prefix/.previous.XXXXXXXX") || fail 'Could not allocate a backup directory in the prefix.'
    publishing=true
    for entry in "${entries[@]}"; do
        if [[ -e $prefix/$entry ]]; then
            move_stage_entry "$prefix/$entry" "$backup/$entry"
        fi
        mv -T -- "$candidate/$entry" "$prefix/$entry"
    done
    chmod -R a+rX,a-w -- "${staged_paths[@]}"
}

cleanup() {
    local result=$? entry recovered=true directory
    trap - EXIT ERR
    set +e
    if $publishing; then
        for entry in "${entries[@]}"; do
            if [[ ! -e $candidate/$entry && -e $prefix/$entry ]]; then
                if ! move_stage_entry "$prefix/$entry" "$candidate/$entry"; then
                    recovered=false
                    continue
                fi
            fi
            if [[ -e $backup/$entry ]]; then
                move_stage_entry "$backup/$entry" "$prefix/$entry" || recovered=false
            fi
        done
        if ! $recovered; then
            printf 'Restore the previous stage manually from %s. New files are in %s or %s.\n' "$backup" "$candidate" "$prefix" >&2
            exit 1
        fi
    fi
    for directory in "$candidate" "$backup" "$seed"; do
        [[ -n $directory ]] || continue
        remove_tree "$directory" || {
            printf 'Could not clean up %s. Remove it before discarding this report.\n' "$directory" >&2
            result=1
        }
    done
    exit "$result"
}

self_test() (
    (( EUID != 0 )) || fail 'Run the self-test as an unprivileged user to check directory permissions.'
    test_dir=$(mktemp -d "${TMPDIR:-/tmp}/fuzukue-android-stage-test.XXXXXXXX") ||
        fail 'Could not allocate a self-test directory under TMPDIR.'
    trap 'remove_tree "$test_dir"' EXIT
    prefix=$test_dir/prefix
    leftover=$prefix/.candidate.leftover
    mkdir -p -- "$leftover/jdk/bin" "$test_dir/keep"
    touch -- "$leftover/jdk/bin/java" "$test_dir/keep/sentinel"
    ln -s -- "$test_dir/keep" "$leftover/link"
    chmod -R a-w -- "$leftover"
    dry_run=false
    assume_yes=true
    clean_leftover_candidates
    [[ ! -e $leftover && -f $test_dir/keep/sentinel && -w $test_dir/keep ]] ||
        fail 'Leftover cleanup did not stay within the candidate.'

    candidate=$(mktemp -d "$prefix/.candidate.XXXXXXXX")
    for directory in "$prefix" "$candidate"; do
        for entry in "${entries[@]}"; do
            if [[ $entry == STAGED.txt ]]; then
                printf '%s\n' "$directory" > "$directory/$entry"
            else
                mkdir -p -- "$directory/$entry/nested"
                printf '%s\n' "$directory" > "$directory/$entry/nested/data"
            fi
        done
    done
    chmod -R a-w -- "$prefix"/* "$candidate/sdk"

    # Simulate verification failure and let the normal EXIT trap restore the old stage.
    set +e
    (
        set -e
        trap cleanup EXIT
        publish_stage
        [[ $(<"$prefix/STAGED.txt") == "$candidate" && -w $prefix ]] ||
            fail 'The candidate was not published into the writable prefix.'
        for entry in "${entries[@]}"; do
            [[ -e $prefix/$entry && ! -w $prefix/$entry ]] ||
                fail "Published entry was not sealed: $entry"
        done
        [[ ! -w $prefix/jdk/nested ]] || fail 'Published nested directory was not sealed.'
        exit 42
    )
    result=$?
    set -e
    [[ $result -eq 42 && $(<"$prefix/STAGED.txt") == "$prefix" ]] ||
        fail 'Rollback did not restore the previous stage and preserve the failure status.'
    for entry in "${entries[@]}"; do
        [[ -e $prefix/$entry && ! -w $prefix/$entry ]] ||
            fail "Previous entry was not restored read-only: $entry"
        if [[ -d $prefix/$entry ]]; then
            [[ $(<"$prefix/$entry/nested/data") == "$prefix" ]] ||
                fail "Previous entry contents were not restored: $entry"
        fi
    done
    shopt -s nullglob
    leftovers=("$prefix"/.candidate.* "$prefix"/.previous.*)
    ((${#leftovers[@]} == 0)) || fail 'Rollback left a candidate or backup behind.'
    printf 'Self-test passed: leftover cleanup, publication, sealing and rollback.\n'
)

if $run_self_test; then
    self_test
    exit
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
android=$repo/android
jdk=${JAVA_HOME:-$HOME/opt/jdk-21.0.13.1+2}
sdk=${ANDROID_HOME:-$HOME/Android/Sdk}
temp_base=${TMPDIR:-/tmp}
sdk_packages=(platforms/android-37.0 build-tools/36.0.0 platform-tools)
wrapper_properties=gradle/wrapper/gradle-wrapper.properties
wrapper_dist=$(sed -nE 's|^distributionUrl=.*/(gradle-[0-9][0-9A-Za-z.-]*-bin)\.zip\r?$|\1|p' "$android/$wrapper_properties") ||
    fail "Could not read android/$wrapper_properties. Use a complete checkout."
[[ $wrapper_dist =~ ^gradle-[0-9][0-9A-Za-z.-]*-bin$ ]] || fail "Expected one Gradle -bin distribution in android/$wrapper_properties."
gradle_version=${wrapper_dist#gradle-}
gradle_version=${gradle_version%-bin}
prefix=$(realpath -m -- "$prefix") || fail 'Could not resolve --prefix.'
# Keep the path safe in both the TOML and the JVM option list.
[[ $prefix =~ ^/[A-Za-z0-9._+/-]+$ ]] || fail 'Choose a prefix using only letters, digits, dots, underscores, plus signs, hyphens and slashes.'
[[ $prefix != "$repo" && $prefix != "$repo/"* && $repo != "$prefix/"* ]] || fail 'Choose a dedicated prefix outside the checkout.'

probe=(/usr/bin/setpriv --no-new-privs /usr/bin/bwrap
    --unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts
    --disable-userns --cap-drop ALL --proc /proc --dev /dev
    --ro-bind /usr /usr --ro-bind-try /bin /bin --ro-bind-try /lib /lib
    --ro-bind-try /lib64 /lib64 --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache
    -- /usr/bin/true)
offline_options="-Drobolectric.offline=true -Drobolectric.dependency.dir=$prefix/robolectric"
# The read-only distribution keeps its executable bits but needs writable
# directories for the wrapper's per-run lock file and any wrapper bookkeeping.
# shellcheck disable=SC2016
recipe_command='export JAVA_TOOL_OPTIONS="$JAVA_TOOL_OPTIONS -Duser.home=\"$HOME\""; mkdir -p "$GRADLE_USER_HOME/wrapper/dists" && cp -R "${JAVA_HOME%/jdk}/wrapper/dists/." "$GRADLE_USER_HOME/wrapper/dists/" && chmod -R u+w "$GRADLE_USER_HOME/wrapper/dists" && exec ./gradlew --offline --no-daemon :app:testDebugUnitTest'

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'printf "Error in %s (line %s). Check the command output, path permissions and free disk space.\n" "$step_name" "$LINENO" >&2' ERR

step '1. Preflight' "Check JDK 21 (java and javac) at $jdk." \
    "Check SDK packages and accepted licenses at $sdk: ${sdk_packages[*]}." \
    'Check the checkout for android/keystore.properties and check the service account.' \
    'Check bubblewrap for --bind-fd and run the isolation probe as the current user.'
show_probe
if ! $dry_run; then
    (( EUID != 0 )) || fail 'Run the helper as the unprivileged staging operator.'
    [[ ! -e $android/keystore.properties && ! -L $android/keystore.properties ]] ||
        fail 'Move android/keystore.properties and signing keys outside the workspace, then rerun.'
    for tool in flock git python3 sha256sum tar; do
        command -v "$tool" >/dev/null || fail "Install $tool or make it available on PATH."
    done
    jdk=$(realpath -e -- "$jdk") || fail 'Set JAVA_HOME to a full Linux JDK 21 (see android/README.md).'
    sdk=$(realpath -e -- "$sdk") || fail 'Set ANDROID_HOME to the installed Android SDK.'
    [[ -x $jdk/bin/java && -x $jdk/bin/javac && -r $jdk/release ]] || fail 'Set JAVA_HOME to a full JDK 21 with javac and a release file.'
    javac_version=$(env -i PATH=/usr/bin:/bin "$jdk/bin/javac" -version) || fail 'javac did not start. Check the JDK and host libraries.'
    [[ $javac_version == 'javac 21' || $javac_version == 'javac 21.'* ]] || fail "Expected JDK 21, found $javac_version."
    [[ -r $sdk/platforms/android-37.0/android.jar && -x $sdk/build-tools/36.0.0/aapt2 && -x $sdk/platform-tools/adb ]] ||
        fail 'Use SDK Manager to install platforms;android-37.0, build-tools;36.0.0 and platform-tools.'
    check_links "$jdk" "$jdk"
    for part in "${sdk_packages[@]}" licenses; do
        check_links "$sdk" "$sdk/$part"
    done
    check_sdk_licenses || fail "Review and accept licenses in Android Studio's SDK Manager or run sdkmanager --licenses yourself."
    service_note
    [[ -x ${probe[0]} && -x ${probe[2]} ]] || fail 'Make /usr/bin/setpriv and /usr/bin/bwrap available on the recipe host.'
    bwrap_help=$(/usr/bin/bwrap --help 2>&1) || fail 'Could not query /usr/bin/bwrap. Check the installed binary.'
    [[ $bwrap_help == *--bind-fd* ]] || fail 'Use a bubblewrap build with --bind-fd support.'
    "${probe[@]}" || fail 'The isolation probe failed. Check unprivileged user namespace support and host policy for bubblewrap.'
    printf '  Isolation probe passed as %s.\n' "$(id -un)"
fi

step '2. Prepare the staging prefix' "Create or reuse the operator-owned directory $prefix." \
    'Lock the prefix against concurrent staging and check for unrelated content.' \
    'On re-staging, keep the Android recipe queue idle until verification and publication finish.'
case $prefix in
    /usr/local/share/*)
        if [[ ! -e $prefix && ! -w ${prefix%/*}/ ]]; then
            printf '  If an administrator must create this directory, run this command yourself and rerun the helper.\n'
            sudo_hint
        fi
        ;;
    /usr/*|/bin/*|/sbin/*|/lib/*|/lib64/*) ;;
    *) printf '  This prefix is usable for a host check only. The broker cannot see it in a read-only system mount. Use an operator-owned prefix under /usr/local/share to enable the recipe.\n' ;;
esac
if ! $dry_run; then
    [[ $prefix != "$jdk" && $prefix != "$jdk/"* && $prefix != "$sdk" && $prefix != "$sdk/"* ]] || fail 'Choose a prefix outside the source JDK and SDK.'
    if [[ ! -e $prefix ]]; then
        [[ -w ${prefix%/*}/ && -x ${prefix%/*}/ ]] || fail 'The prefix parent is not writable. Use an operator-owned --prefix or the administrator command above if shown.'
        install -d -m 0755 -- "$prefix" || fail 'Could not create the staging prefix. Check its parent permissions.'
    fi
    [[ -d $prefix && -O $prefix && -w $prefix && -x $prefix ]] || fail 'Use a writable directory owned by the staging operator.'
    exec {stage_lock}<"$prefix"
    flock -n "$stage_lock" || fail "Another staging operation is using $prefix. Retry when it finishes."
    for entry in "${entries[@]}"; do
        if [[ -e $prefix/$entry || -L $prefix/$entry ]]; then
            [[ ! -L $prefix/$entry && -f $prefix/STAGED.txt && ! -L $prefix/STAGED.txt ]] &&
                [[ $(head -n 1 -- "$prefix/STAGED.txt") == 'fuzukue android-test stage' ]] ||
                fail "Unrecognized staging content at $prefix/$entry. Choose a dedicated prefix or move the existing content aside."
        fi
    done
    clean_leftover_candidates
fi

step '3. Copy the JDK and SDK' "Copy $jdk and the required SDK packages and licenses into a candidate directory under $prefix." \
    'Materialize internal toolchain links as ordinary files and directories.'
if ! $dry_run; then
    candidate=$(mktemp -d "$prefix/.candidate.XXXXXXXX") || fail 'Could not allocate a candidate directory in the prefix.'
    cp -RL -- "$jdk" "$candidate/jdk"
    for part in "${sdk_packages[@]}" licenses; do
        mkdir -p -- "$(dirname -- "$candidate/sdk/$part")"
        cp -RL -- "$sdk/$part" "$candidate/sdk/$part"
    done
fi

step '4. Warm up online' "Create a seed directory under $temp_base, outside the checkout and prefix." \
    'Make two fresh copies of the tracked android/ sources, leaving out local.properties and keystore.properties.' \
    'Clear the inherited environment, including GRADLE_RO_DEP_CACHE and signing credentials.' \
    'Use the copied JDK and SDK and put HOME, Gradle user home and Maven local under the seed directory.' \
    'From the warm copy of android/, run ./gradlew --no-daemon --rerun-tasks :app:testDebugUnitTest.'
if ! $dry_run; then
    temp_base=$(realpath -e -- "$temp_base") || fail 'Set TMPDIR to an existing writable directory outside the checkout and prefix.'
    [[ $temp_base != "$repo" && $temp_base != "$repo/"* && $temp_base != "$prefix" && $temp_base != "$prefix/"* && $temp_base != *\"* && $temp_base != *\\* && $temp_base != *$'\n'* ]] ||
        fail 'Set TMPDIR to a writable directory outside the checkout and prefix without quotes, backslashes or newlines.'
    seed=$(mktemp -d "$temp_base/fuzukue-android-stage.XXXXXXXX") || fail 'Could not create a seed directory. Check TMPDIR permissions and free space.'
    mkdir -p -- "$seed"/{warm,cold,home,gradle,m2,tmp}
    git -C "$repo" ls-files -z -- android/ |
        tar -C "$repo" --exclude=android/local.properties --exclude=android/keystore.properties --null --no-recursion -T - -cf - |
        tar -C "$seed/warm" -xf -
    [[ -x $seed/warm/android/gradlew && -z $(find "$seed/warm/android" -type l -print -quit) ]] ||
        fail 'Use a checkout with a tracked, executable Gradle wrapper and ordinary Android source files.'
    cp -R -- "$seed/warm/android" "$seed/cold/android"
    (
        cd -- "$seed/warm/android"
        env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME="$seed/home" TMPDIR="$seed/tmp" \
            JAVA_HOME="$candidate/jdk" ANDROID_HOME="$candidate/sdk" GRADLE_USER_HOME="$seed/gradle" \
            JAVA_TOOL_OPTIONS="-Duser.home=\"$seed/home\" -Dmaven.repo.local=\"$seed/m2\"" \
            ./gradlew --no-daemon --rerun-tasks :app:testDebugUnitTest
    ) || fail 'Warm-up failed. Check the Gradle output, network access, SDK requirements and test results.'
fi

step '5. Export the caches' "Copy $wrapper_dist and its .ok marker into wrapper/dists." \
    'Drop transient wrapper locks. Copy caches/modules-2 into gradle-deps, excluding *.lock and gc.properties.' \
    'Copy the Robolectric Android framework JARs from seed/m2/org/robolectric directly into robolectric/.'
if ! $dry_run; then
    shopt -s nullglob
    dist_dir=$seed/gradle/wrapper/dists/$wrapper_dist
    markers=("$dist_dir"/*/"$wrapper_dist.zip.ok")
    [[ ${#markers[@]} -eq 1 && -f ${markers[0]} ]] ||
        fail "Expected one .ok marker for $wrapper_dist. Check the wrapper warm-up."
    marker=${markers[0]#"$seed/gradle/"}
    launcher=${marker%/*}/gradle-$gradle_version/lib/gradle-launcher-$gradle_version.jar
    [[ -f $seed/gradle/$launcher ]] || fail "The expanded $wrapper_dist is missing its launcher JAR. Check the wrapper warm-up."
    [[ -d $seed/gradle/caches/modules-2/files-2.1 ]] || fail 'Gradle did not populate caches/modules-2. Check the warm-up output.'
    jars=("$seed/m2/org/robolectric"/android-all*/*/android-all*.jar)
    ((${#jars[@]})) || fail 'No Robolectric Android framework JARs were downloaded. Check that Robolectric tests ran during warm-up.'
    mkdir -p -- "$candidate"/{wrapper/dists,gradle-deps,robolectric}
    cp -R -- "$dist_dir" "$candidate/wrapper/dists/"
    find "$candidate/wrapper/dists" -type f -name '*.lck' -delete
    tar -C "$seed/gradle/caches" --exclude='*.lock' --exclude=gc.properties -cf - modules-2 |
        tar -C "$candidate/gradle-deps" -xf -
    cp -- "${jars[@]}" "$candidate/robolectric/"
    [[ -z $(find "$candidate" -type l -print -quit) ]] || fail 'The staged tree contains a symlink. Inspect the toolchain and downloaded caches.'
fi

step '6. Record the stage' 'Write STAGED.txt with the UTC date, toolchain versions and SHA-256 of key files.'
if ! $dry_run; then
    (
        cd -- "$candidate"
        printf 'fuzukue android-test stage\nStaged at %s\nGradle %s\nJDK %s\n' "$(date -u +%FT%TZ)" "$gradle_version" "$javac_version"
        for part in "${sdk_packages[@]}"; do
            printf 'SDK %s\n' "$part"
            sed -n '/^Pkg.Revision[[:space:]]*=/p' "sdk/$part/source.properties"
        done
        printf '\nSHA-256 of staged files\n'
        sha256sum -- jdk/{bin/java,bin/javac,release} \
            sdk/{platforms/android-37.0/android.jar,build-tools/36.0.0/aapt2,platform-tools/adb} \
            "$marker" "$launcher" robolectric/*.jar
        printf '\nSHA-256 of checkout wrapper inputs\n'
        (cd -- "$seed/cold/android" && sha256sum -- gradlew "$wrapper_properties" gradle/wrapper/gradle-wrapper.jar)
    ) > "$candidate/STAGED.txt"
fi

step '7. Verify cold and offline' "Move the candidate into $prefix. Restore any previous stage if verification fails." \
    "Seal the staged contents after moving them. Keep $prefix writable for the operator." \
    'Use a fresh Android project copy, HOME and Gradle user directory outside the broker.' \
    "Set JAVA_HOME=$prefix/jdk, ANDROID_HOME=$prefix/sdk, GRADLE_RO_DEP_CACHE=$prefix/gradle-deps." \
    "Set JAVA_TOOL_OPTIONS=$offline_options" \
    "Run the fixed recipe command: $recipe_command"
if ! $dry_run; then
    publish_stage
    mkdir -p -- "$seed/offline"/{home,gradle,tmp}
    (
        cd -- "$seed/cold/android"
        env -i PATH=/usr/bin:/bin LANG=C.UTF-8 HOME="$seed/offline/home" TMPDIR="$seed/offline/tmp" \
            JAVA_HOME="$prefix/jdk" ANDROID_HOME="$prefix/sdk" GRADLE_RO_DEP_CACHE="$prefix/gradle-deps" \
            GRADLE_USER_HOME="$seed/offline/gradle" JAVA_TOOL_OPTIONS="$offline_options" \
            /bin/sh -c "$recipe_command"
    ) || fail 'Cold offline verification failed. Review the Gradle output for missing caches or failing tests before re-staging.'
    publishing=false
    printf '  Cold offline verification passed.\n'
fi

step '8. Print the recipe and activation instructions' 'Apply the printed configuration yourself after successful staging.' \
    'On exit, remove the seed, candidate and any superseded stage backup.'
# Only backslashes and double quotes need escaping in this fixed TOML command.
toml_command=${recipe_command//\\/\\\\}
toml_command=${toml_command//\"/\\\"}
cat <<EOF

[recipes.android-test]
command = ["/bin/sh", "-c", "$toml_command"]
cwd = "android"
env = { JAVA_HOME = "$prefix/jdk", ANDROID_HOME = "$prefix/sdk", GRADLE_RO_DEP_CACHE = "$prefix/gradle-deps", GRADLE_USER_HOME = "/recipe/runtime/gradle", JAVA_TOOL_OPTIONS = "$offline_options" }
timeout_secs = 1200
max_memory_mib = 8192
queue = "gradle"

1. After a real staging pass, append this block to the machine-local fuzukue.toml
   selected by FUZUKUE_SERVER_CONFIG in the server's launch environment. Replace
   an existing [recipes.android-test] block. A custom --prefix is reflected in
   the env paths above and must be visible in a read-only broker mount.
2. Keep android/local.properties with a host sdk.dir, signing material and
   credentials outside the mounted workspace. Confirm the isolation probe under
   the service account and leave recipe_runner.allow_unisolated disabled.
3. Restart the server later using the normal update/restart path (docs/update.md
   and docs/service.md). For a systemd user service, use secretaire service restart.
4. First run: run_recipe android-test (MCP: run_recipe({"name":"android-test"})).
EOF
