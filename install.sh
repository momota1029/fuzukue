#!/bin/sh
# Keep all work inside the function. The final compound must also parse in full.
install_fuzukue() (
    set -u
    umask 077
    incoming_path=${PATH:-}
    PATH=/usr/bin:/bin:/usr/sbin:/sbin
    export PATH
    locale=${LC_ALL:-${LC_MESSAGES:-${LANG:-}}}
    if [ -z "$locale" ]; then locale=$(defaults read -g AppleLocale 2>/dev/null) || locale=; fi
    say() {
        case $locale in ja*) printf '%s\n' "$1" ;; *) printf '%s\n' "$2" ;; esac
    }
    fail() { say "$@" >&2; exit 1; }
    usage() {
        say '使い方：引数なしでインストール、--uninstall で削除、--no-auto-update で自動更新を停止。' \
            'Run without arguments to install, with --uninstall to remove, or with --no-auto-update to pause updates.'
    }

    mode=install fixture= stage_only=false skip_agents=false port=7878
    while [ "$#" -gt 0 ]; do
        case $1 in
            --uninstall|--update|--no-auto-update|--resume-auto-update|--guide) mode=${1#--} ;;
            --help) usage; exit 0 ;;
            --fixture) shift; fixture=${1:-}; [ -n "$fixture" ] || exit 2 ;;
            --stage-only) stage_only=true ;;
            --skip-agents) skip_agents=true ;;
            --test-port) shift; port=${1:-} ;;
            *) usage >&2; exit 2 ;;
        esac
        shift
    done
    if [ -z "$fixture" ] && { "$stage_only" || "$skip_agents" || [ "$port" != 7878 ]; }; then
        usage >&2
        exit 2
    fi
    case $port in ''|*[!0-9]*) exit 2 ;; esac
    [ "$port" -ge 1024 ] && [ "$port" -le 65535 ] || exit 2
    [ "$(uname -s)" = Darwin ] ||
        fail 'このインストーラーは Mac 用です。Mac のターミナルで実行してください。' \
             'This installer is for Mac. Run it in Terminal on your Mac.'
    os_version=$(sw_vers -productVersion)
    [ "${os_version%%.*}" -ge 15 ] 2>/dev/null ||
        fail 'macOS 15 以降が必要です。「システム設定」の「ソフトウェアアップデート」を開いてください。' \
             'macOS 15 or later is required. Open Software Update in System Settings.'
    uid=$(id -u)
    [ "$uid" != 0 ] ||
        fail 'ふだん使うアカウントで、sudo を付けずに実行してください。' 'Run this from your own account without sudo.'
    case ${HOME:-} in /*) ;; *) exit 1 ;; esac
    case "$(uname -m):$(sysctl -n hw.optional.arm64 2>/dev/null)" in
        arm64:*|x86_64:1) platform=macos-aarch64 ;;
        x86_64:*) platform=macos-x86_64 ;;
        *) fail 'この Mac に対応する配布版がありません。' 'There is no package for this Mac.' ;;
    esac

    repo=momota1029/fuzukue
    base="$HOME/Library/Application Support/fuzukue/desktop-install"
    applications="$HOME/Applications"
    app="$applications/fuzukue.app"
    agents="$HOME/Library/LaunchAgents"
    logs="$HOME/Library/Logs/fuzukue"
    project="$HOME/fuzukue-project"
    server_label=com.fuzukue.desktop.server
    update_label=com.fuzukue.desktop.update
    server_unit="$agents/$server_label.plist"
    update_unit="$agents/$update_label.plist"
    domain="gui/$uid"
    endpoint="http://127.0.0.1:$port"
    support_script="$app/Contents/Resources/install.sh"
    gh_bin="$base/bin"
    if [ -n "$fixture" ]; then
        fixture=$(cd "$fixture" && pwd -P) || exit 2
    else
        # Use the same system developer-tools selection as the LaunchAgent.
        unset DEVELOPER_DIR
    fi

    local_error() {
        fail '準備を完了できませんでした。同じインストールコマンドをもう一度実行してください。' \
             'Setup could not finish. Run the same install command again.'
    }
    network_error() {
        fail 'ダウンロードできませんでした。インターネット接続を確認してやり直してください。' \
             'The download failed. Check your internet connection and try again.'
    }
    verify_error() {
        fail '配布ファイルを確認できませんでした。配布元の GitHub Releases ページで公開状況を確認してください。' \
             'The release files could not be verified. Check the project’s GitHub Releases page.'
    }
    occupied_error() {
        fail '別の fuzukue が動いています。そちらを終了してからもう一度実行してください。' \
             'Another fuzukue is running. Stop it and run the installer again.'
    }
    fetch() {
        curl --disable --fail --silent --location --proto '=https' --proto-redir '=https' \
            --max-redirs 10 --connect-timeout 15 --max-time 600 --output "$2" "$1" </dev/null
    }
    loopback() {
        curl --disable --noproxy '*' --fail --silent --max-time 2 "$endpoint/$1" </dev/null
    }

    # Apple’s JavaScript for Automation reads real JSON, including GitHub's null fields.
    metadata() {
        osascript -l JavaScript - "$@" <<'JXA'
ObjC.import("Foundation");

function readJSON(path) {
    const text = $.NSString.stringWithContentsOfFileEncodingError(
        path, $.NSUTF8StringEncoding, null);
    return JSON.parse(ObjC.unwrap(text));
}

const number = "(?:0|[1-9][0-9]*)";
const identifier = "(?:" + number + "|[0-9]*[A-Za-z-][0-9A-Za-z-]*)";
const version = new RegExp("^v(" + number + ")\\.(" + number + ")\\.(" + number +
    ")(?:-(" + identifier + "(?:\\." + identifier + ")*))?$");
const commit = /^[0-9a-f]{40}$/;
const digest = /^[0-9a-f]{64}$/;
const manifestName = "macos-release-manifest.json";
const sumsName = "SHA256SUMS-macos";

function requireValue(condition) {
    if (!condition) throw Error("Invalid fuzukue release metadata");
}

function compare(a, b) {
    return a === b ? 0 : a > b ? 1 : -1;
}

function compareNumber(a, b) {
    return compare(a.length, b.length) || compare(a, b);
}

function compareVersions(a, b) {
    const left = version.exec(a), right = version.exec(b);
    for (let i = 1; i <= 3; i++) {
        const order = compareNumber(left[i], right[i]);
        if (order) return order;
    }
    if (!left[4] || !right[4]) return compare(!left[4], !right[4]);
    const x = left[4].split("."), y = right[4].split(".");
    for (let i = 0; i < Math.min(x.length, y.length); i++) {
        const xn = /^[0-9]+$/.test(x[i]), yn = /^[0-9]+$/.test(y[i]);
        const order = xn && yn ? compareNumber(x[i], y[i]) :
            xn !== yn ? compare(yn, xn) : compare(x[i], y[i]);
        if (order) return order;
    }
    return compare(x.length, y.length);
}

function packageNames(tag, platform) {
    return ["secretaire-" + tag + "-" + platform + ".tar.gz",
            "fuzukue-app-" + tag + "-" + platform + ".tar.gz"];
}

function run(args) {
    const [action, path, first, second, third] = args;
    const data = readJSON(path);
    switch (action) {
    case "releases": {
        const [platform, current, previousBest] = [first, second, third];
        const installed = current && version.exec(current);
        requireValue(Array.isArray(data) && (!current || installed));
        let best = previousBest;
        for (const release of data) {
            const tag = release.tag_name;
            const match = typeof tag === "string" && version.exec(tag);
            if (!match || match[0] !== tag || release.draft !== false ||
                release.prerelease !== !!match[4]) continue;
            if (installed && !installed[4] && match[4]) continue;
            const names = packageNames(tag, platform).concat([manifestName, sumsName]);
            if (!Array.isArray(release.assets) ||
                !names.every(name => release.assets.filter(asset => asset.name === name).length === 1)) continue;
            if ((!current || compareVersions(tag, current) >= 0) &&
                (!best || compareVersions(tag, best) > 0)) best = tag;
        }
        return (best || "-") + " " + data.length;
    }
    case "ref":
    case "tag": {
        if (action === "ref") requireValue(data.ref === "refs/tags/" + first);
        else requireValue(data.sha === first);
        const obj = data.object;
        requireValue(obj && commit.test(obj.sha) && ["commit", "tag"].includes(obj.type));
        return obj.type + " " + obj.sha;
    }
    case "manifest": {
        const [tag, sha, platform] = [first, second, third];
        requireValue(data.schema_version === 1 && data.version === tag.slice(1) &&
            data.commit === sha && data.channel === (version.exec(tag)[4] ? "beta" : "stable"));
        requireValue(Number.isSafeInteger(data.protocol) &&
            [data.min_client_protocol, data.min_server_protocol].every(n =>
                Number.isSafeInteger(n) && n > 0 && n <= data.protocol));
        requireValue(Array.isArray(data.artifacts));
        return packageNames(tag, platform).map((name, i) => {
            const kind = i ? "macos-app" : "server";
            const items = data.artifacts.filter(x => x.kind === kind && x.platform === platform);
            requireValue(items.length === 1);
            const item = items[0];
            requireValue(item.name === name && Number.isSafeInteger(item.size) &&
                item.size > 0 && digest.test(item.sha256));
            return item.name + " " + item.size + " " + item.sha256;
        }).join("\n");
    }
    default:
        throw Error("Unknown metadata action");
    }
}
JXA
    }
    get_api() {
        if [ -n "$fixture" ]; then
            cp "$fixture/api/$2.json" "$work/api.json"
        else
            fetch "https://api.github.com/repos/$repo/$1" "$work/api.json"
        fi
    }
    select_release() {
        installed=
        if [ -f "$base/release" ]; then
            IFS= read -r installed < "$base/release" || local_error
        fi
        tag= page=1
        while [ "$page" -le 1000 ]; do
            get_api "releases?per_page=100&page=$page" "releases-$page" || network_error
            row=$(metadata releases "$work/api.json" "$platform" "$installed" "$tag" 2>/dev/null) || verify_error
            tag=${row% *}
            [ "$tag" != - ] || tag=
            [ "${row##* }" = 100 ] || break
            page=$((page + 1))
        done
        [ "$page" -le 1000 ] || verify_error
        [ -n "$tag" ] ||
            fail 'この Mac 向けの公開版がまだありません。GitHub Releases でベータ版の公開を待ってください。' \
                 'There is no published package for this Mac yet. Check GitHub Releases for the beta.'

        get_api "git/ref/tags/$tag" "ref-$tag" || network_error
        object=$(metadata ref "$work/api.json" "$tag" 2>/dev/null) || verify_error
        depth=0
        while [ "${object% *}" = tag ] && [ "$depth" -lt 10 ]; do
            sha=${object#* }
            get_api "git/tags/$sha" "tag-$sha" || network_error
            object=$(metadata tag "$work/api.json" "$sha" 2>/dev/null) || verify_error
            depth=$((depth + 1))
        done
        [ "${object% *}" = commit ] || verify_error
        commit=${object#* }
    }
    get_asset() {
        if [ -n "$fixture" ]; then
            cp "$fixture/assets/$tag/$1" "$work/$1"
        else
            fetch "https://github.com/$repo/releases/download/$tag/$1" "$work/$1"
        fi
    }
    checksum() {
        output=$(shasum -a 256 < "$1") || return
        printf '%s\n' "${output%% *}"
    }
    verify_sum() {
        expected=$(awk -v name="$1" '$2 == name { hash = $1; count++ }
            END { if (count != 1 || length(hash) != 64 || hash ~ /[^0-9a-f]/) exit 1; print hash }' \
            "$work/SHA256SUMS-macos") || verify_error
        actual=$(checksum "$work/$1") || verify_error
        [ "$actual" = "$expected" ] && [ "$actual" = "${2:-$expected}" ] || verify_error
    }
    download_packages() {
        get_asset SHA256SUMS-macos && get_asset macos-release-manifest.json || network_error
        verify_sum macos-release-manifest.json
        packages=$(metadata manifest "$work/macos-release-manifest.json" "$tag" "$commit" "$platform" 2>/dev/null) ||
            verify_error
        while IFS=' ' read -r name size sha; do
            get_asset "$name" || network_error
            [ "$(stat -f '%z' "$work/$name")" = "$size" ] || verify_error
            verify_sum "$name" "$sha"
        done <<PACKAGES
$packages
PACKAGES
    }
    unpack() {
        archive="$work/secretaire-$tag-$platform.tar.gz"
        tar -tvzf "$archive" > "$work/entries" 2>/dev/null || verify_error
        awk '$NF == "secretaire" { if (substr($0,1,1) != "-") exit 1; n++ }
            END { if (n != 1) exit 1 }' "$work/entries" || verify_error
        tar -xzf "$archive" -C "$work" secretaire 2>/dev/null || verify_error
        [ -f "$work/secretaire" ] && [ ! -L "$work/secretaire" ] && [ -x "$work/secretaire" ] || verify_error

        archive="$work/fuzukue-app-$tag-$platform.tar.gz"
        tar -tzf "$archive" > "$work/names" 2>/dev/null &&
            tar -tvzf "$archive" > "$work/entries" 2>/dev/null || verify_error
        # Release archives have simple ASCII paths, no links, and exactly one bundle.
        awk '$0 !~ /^fuzukue[.]app(\/[A-Za-z0-9_][A-Za-z0-9_.-]*)*\/?$/ || seen[$0]++ { exit 1 }' \
            "$work/names" || verify_error
        awk 'substr($0,1,1) !~ /^[-d]$/ || / link to / { exit 1 }' "$work/entries" || verify_error
        tar -xzf "$archive" -C "$work" 2>/dev/null || verify_error
        bundle="$work/fuzukue.app"
        [ "$(plist CFBundleIdentifier "$bundle/Contents/Info.plist")" = com.fuzukue.desktop ] &&
            [ "$(plist FuzukueReleaseTag "$bundle/Contents/Info.plist")" = "$tag" ] &&
            [ "$(plist FuzukueReleaseCommit "$bundle/Contents/Info.plist")" = "$commit" ] &&
            [ -s "$bundle/Contents/Resources/fuzukue.icns" ] &&
            [ -f "$bundle/Contents/Resources/install.sh" ] &&
            [ -x "$bundle/Contents/Resources/bin/git" ] &&
            [ -x "$bundle/Contents/MacOS/fuzukue" ] &&
            codesign --verify --strict "$bundle" 2>/dev/null &&
            codesign --verify --strict "$work/secretaire" 2>/dev/null || verify_error
    }

    plist() { plutil -extract "$1" raw -o - "$2" 2>/dev/null; }
    owned_unit() { [ "$(plist FuzukueInstaller "$1")" = one-line ]; }
    loaded() { launchctl print "$domain/$1" >/dev/null 2>&1; }
    owned_job() {
        status=$(launchctl print "$domain/$1" 2>/dev/null) || return 1
        printf '%s\n' "$status" | FUZUKUE_UNIT_PATH="$2" awk '
            /^[[:space:]]*path = / {
                sub(/^[[:space:]]*path = /, ""); if ($0 == ENVIRON["FUZUKUE_UNIT_PATH"]) found = 1
            }
            END { exit !found }'
    }
    stop_job() {
        launchctl bootout "$domain/$1" >/dev/null 2>&1 || return 1
        tries=0
        while loaded "$1" && [ "$tries" -lt 85 ]; do
            sleep 1
            tries=$((tries + 1))
        done
        ! loaded "$1"
    }
    healthy_pid() {
        status=$(launchctl print "$domain/$server_label" 2>/dev/null) || return 1
        pid=$(printf '%s\n' "$status" | awk '/^[[:space:]]*pid = [1-9][0-9]*$/ { print $NF; exit }')
        [ -n "$pid" ] && [ "$(loopback self-update/health)" = "$pid" ] && printf '%s\n' "$pid"
    }
    wait_healthy() {
        tries=0
        until healthy_pid >/dev/null; do
            [ "$tries" -lt 30 ] || return 1
            sleep 1
            tries=$((tries + 1))
        done
    }

    write_server_unit() {
        file=$1
        plutil -create xml1 "$file" &&
            plutil -insert Label -string "$server_label" "$file" &&
            plutil -insert FuzukueInstaller -string one-line "$file" &&
            plutil -insert ProgramArguments -json '["", ""]' "$file" &&
            plutil -replace ProgramArguments.0 -string "$base/secretaire" "$file" &&
            plutil -replace ProgramArguments.1 -string "$project" "$file" &&
            plutil -insert WorkingDirectory -string "$project" "$file" &&
            plutil -insert EnvironmentVariables -json '{}' "$file" &&
            plutil -insert EnvironmentVariables.HOME -string "$HOME" "$file" &&
            plutil -insert EnvironmentVariables.PATH -string "$app/Contents/Resources/bin:$cli_path" "$file" &&
            plutil -insert EnvironmentVariables.FUZUKUE_ADDR -string "127.0.0.1:$port" "$file" &&
            plutil -insert EnvironmentVariables.FUZUKUE_MACOS_PACKAGE -string 1 "$file" &&
            plutil -insert RunAtLoad -bool YES "$file" &&
            plutil -insert KeepAlive -json '{"SuccessfulExit":false}' "$file" &&
            plutil -insert ExitTimeOut -integer 75 "$file" &&
            plutil -insert Umask -integer 63 "$file" &&
            plutil -insert StandardOutPath -string "$logs/desktop-server.log" "$file" &&
            plutil -insert StandardErrorPath -string "$logs/desktop-server.err.log" "$file" &&
            plutil -lint -s "$file"
    }
    write_update_unit() {
        file=$1
        plutil -create xml1 "$file" &&
            plutil -insert Label -string "$update_label" "$file" &&
            plutil -insert FuzukueInstaller -string one-line "$file" &&
            plutil -insert ProgramArguments -json '["/bin/sh", "", "--update"]' "$file" &&
            plutil -replace ProgramArguments.1 -string "$support_script" "$file" &&
            plutil -insert EnvironmentVariables -json '{}' "$file" &&
            plutil -insert EnvironmentVariables.HOME -string "$HOME" "$file" &&
            plutil -insert ProcessType -string Background "$file" &&
            plutil -insert StartInterval -integer 21600 "$file" &&
            plutil -insert Umask -integer 63 "$file" &&
            plutil -insert StandardOutPath -string "$logs/desktop-update.log" "$file" &&
            plutil -insert StandardErrorPath -string "$logs/desktop-update.err.log" "$file" &&
            plutil -lint -s "$file"
    }

    cli_path_for() {
        # Store only absolute entries. Preserve paths such as nvm and Volta for the GUI session.
        path="$1:$HOME/.local/bin:$HOME/bin:$HOME/.npm-global/bin:$HOME/.volta/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
        cli_path=
        if [ -x "$gh_bin/gh" ]; then cli_path=$gh_bin; fi
        while [ -n "$path" ]; do
            dir=${path%%:*}
            case $path in *:*) path=${path#*:} ;; *) path= ;; esac
            case $dir in
                /*) case ":$cli_path:" in *":$dir:"*) ;; *) cli_path="${cli_path:+$cli_path:}$dir" ;; esac ;;
            esac
        done
    }
    probe_cli() {
        backend=$1
        shift
        (
            PATH="${bundle:-$app}/Contents/Resources/bin:$cli_path"
            export PATH
            command "$backend" "$@" </dev/null >/dev/null 2>&1 &
            probe=$!
            (sleep 8; kill "$probe") >/dev/null 2>&1 &
            timer=$!
            result=0
            wait "$probe" || result=$?
            kill "$timer" 2>/dev/null || :
            wait "$timer" 2>/dev/null || :
            exit "$result"
        )
    }
    login_hint() {
        if probe_cli claude auth status || probe_cli codex login status; then return 1; fi
        if (PATH=$cli_path command -v claude >/dev/null); then
            say '会話を始めるには、ターミナルで次の行を実行して自分のアカウントでログインしてください。' \
                'To start a conversation, run this line in Terminal and sign in to your account.'
            printf '%s\n' 'PATH="$HOME/.local/bin:$PATH" claude auth login'
        elif (PATH=$cli_path command -v codex >/dev/null); then
            say '会話を始めるには、ターミナルで次の行を実行して自分のアカウントでログインしてください。' \
                'To start a conversation, run this line in Terminal and sign in to your account.'
            printf '%s\n' 'PATH="$HOME/.local/bin:$PATH" codex login'
        else
            say '会話を始めるには、インストールコマンドをもう一度実行し、Claude Code か Codex を選んでください。' \
                'To start a conversation, run the install command again and choose Claude Code or Codex.'
        fi
    }
    ask() {
        answer=
        ( : </dev/tty ) 2>/dev/null || return 1
        say "$@" >/dev/tty && IFS= read -r answer </dev/tty
    }
    yes_answer() { case $answer in y|Y|yes|Yes|YES) return 0 ;; *) return 1 ;; esac; }

    git_hint() {
        (PATH=$cli_path "$app/Contents/Resources/bin/git" --version >/dev/null 2>&1) && return 1
        say '変更履歴と取り消しには Git が必要です。ターミナルでインストールコマンドをもう一度実行してください。' \
            'Change history and undo require Git. Run the install command again in Terminal.'
    }
    require_git() {
        git_guard="$bundle/Contents/Resources/bin/git"
        (PATH=$cli_path "$git_guard" --version >/dev/null 2>&1) && return 0
        if ask '原稿の変更履歴を残すには Git が必要です。Apple のコマンドラインツールを入れますか？ macOS の画面が開き、インストールにしばらくかかります。［y: 開く / Enter: 終了］' \
            'Git protects your manuscript history. Install Apple’s command line tools? A macOS dialog will open, and installation may take a while. [y: open / Enter: exit]' &&
            yes_answer; then
            if xcode-select --install </dev/null; then
                fail 'macOS の画面で「インストール」を選んでください。導入が終わったら、ターミナルで同じ fuzukue のインストールコマンドをもう一度実行してください。' \
                     'Choose Install in the macOS dialog. When it finishes, run the same fuzukue install command again in Terminal.'
            fi
            fail 'Apple のインストール画面を開けませんでした。実行中の導入があれば完了を待ち、同じコマンドをもう一度実行してください。' \
                 'The Apple installer could not be opened. If installation is already running, let it finish and run the same command again.'
        fi
        fail 'Git が必要です。Mac の「ターミナル」で同じインストールコマンドを実行し、Apple のツールの導入を選んでください。' \
             'Git is required. Run the same install command in the Mac Terminal and choose to install Apple’s tools.'
    }
    setup_project_git() (
        PATH="$bundle/Contents/Resources/bin:$cli_path"
        export PATH
        # Ignore Git context inherited from the folder where the installer was invoked.
        git_env_keys=$(git rev-parse --local-env-vars) || local_error
        for git_env_key in $git_env_keys; do unset "$git_env_key"; done
        unset GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_NOSYSTEM
        unset GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL EMAIL
        cd "$project" || local_error
        if [ ! -e .git ] && [ ! -L .git ]; then
            git init --quiet --template= || local_error
        fi
        # Compare physical paths on macOS, including when HOME is reached via a symlink.
        repo_root=$(git rev-parse --show-toplevel) || local_error
        [ "$(cd "$repo_root" && pwd -P)" = "$(pwd -P)" ] || local_error
        if [ -z "$(git config --get user.name)" ]; then
            ask '履歴に表示する名前を入力してください。ニックネームでもかまいません。' \
                'Enter the name to show in Git history. A nickname is fine.' || answer=
            case $answer in
                *[![:space:]]*) git config --local --replace-all -- user.name "$answer" || local_error ;;
                *) fail '履歴に表示する名前が必要です。ターミナルで同じインストールコマンドをもう一度実行してください。' \
                        'Git needs a display name. Run the same install command again in Terminal.' ;;
            esac
        fi
        if [ -z "$(git config --get user.email)" ]; then
            git config --local --replace-all user.email fuzukue@users.noreply.invalid || local_error
        fi
        if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
            # Establish a baseline without sweeping up existing files or staged work.
            git -c core.hooksPath=/dev/null -c commit.gpgsign=false commit --quiet \
                --only --allow-empty -m 'Initialize fuzukue project' </dev/null || local_error
            git rev-parse --verify HEAD >/dev/null || local_error
        fi
    )

    gh_version_of() {
        gh_version_output=$(GH_NO_UPDATE_NOTIFIER=1 "$1" --version </dev/null 2>/dev/null) || return 1
        printf '%s\n' "$gh_version_output" | awk '
            NR == 1 && $1 == "gh" && $2 == "version" && $3 ~ /^[0-9]+[.][0-9]+[.][0-9]+$/ { print $3; found=1 }
            END { exit !found }'
    }
    require_gh() {
        # Update the version and both SHA-256 digests together from cli/cli's release.
        gh_version=2.105.0
        case $platform in
            macos-aarch64) gh_arch=arm64; gh_sha=2a5d03103ae444927383b93d9ffd65579a26e2eb900ce1cf11f55258d5319ded ;;
            macos-x86_64) gh_arch=amd64; gh_sha=23602cbccbbdc6285a8520745b714cb927ce2ca621779373c2a20cd8e33fda66 ;;
        esac
        gh_command=$(PATH=$cli_path command -v gh) || gh_command=
        gh_installed=
        if [ -n "$gh_command" ]; then gh_installed=$(gh_version_of "$gh_command") || gh_installed=; fi
        if [ -n "$gh_installed" ]; then
            [ "$gh_command" = "$gh_bin/gh" ] || return 0
            if awk -v have="$gh_installed" -v want="$gh_version" 'BEGIN {
                split(have, h, "."); split(want, w, ".")
                for (i = 1; i <= 3; i++) if (h[i]+0 != w[i]+0) exit !(h[i]+0 > w[i]+0)
            }'; then return 0; fi
            ask "GitHub CLI を公式版 $gh_version に更新しますか？［y: 更新 / Enter: あとで］" \
                "Update GitHub CLI to official version $gh_version? [y: update / Enter: later]" &&
                yes_answer || return 0
        else
            if ! ask "GitHub に要望や変更を送るための GitHub CLI（gh）を入れますか？ 公式版 $gh_version をホームに入れます。［y: インストール / Enter: 終了］" \
                "Install GitHub CLI (gh) so you can send requests and proposed changes to GitHub? Official version $gh_version will be installed in your home folder. [y: install / Enter: exit]" ||
                ! yes_answer; then
                fail 'GitHub CLI（gh）が必要です。ターミナルで同じインストールコマンドを実行し、導入を選んでください。' \
                     'GitHub CLI (gh) is required. Run the same install command in Terminal and choose to install it.'
            fi
        fi
        gh_package="gh_${gh_version}_macOS_$gh_arch"
        if [ -n "$fixture" ]; then
            cp "$fixture/gh/$gh_package.zip" "$work/gh.zip" &&
                IFS= read -r gh_sha < "$fixture/gh/$gh_package.sha256" || network_error
        else
            fetch "https://github.com/cli/cli/releases/download/v$gh_version/$gh_package.zip" "$work/gh.zip" || network_error
        fi
        if [ "$(checksum "$work/gh.zip")" != "$gh_sha" ] ||
            ! unzip -p "$work/gh.zip" "$gh_package/bin/gh" > "$transaction/gh" ||
            ! chmod 755 "$transaction/gh" ||
            [ "$(gh_version_of "$transaction/gh")" != "$gh_version" ]; then
            fail 'GitHub CLI の配布ファイルを確認できませんでした。同じインストールコマンドをもう一度実行してください。' \
                 'The GitHub CLI download could not be verified. Run the same install command again.'
        fi
        [ ! -L "$gh_bin" ] && [ ! -d "$gh_bin/gh" ] &&
            mkdir -p "$gh_bin" && mv -f "$transaction/gh" "$gh_bin/gh" || local_error
        cli_path_for "$cli_path"
    }
    gh_hint() {
        gh_command=$(PATH=$cli_path command -v gh) || {
            say 'GitHub CLI（gh）が見つかりません。ターミナルで fuzukue のインストールコマンドをもう一度実行してください。' \
                'GitHub CLI (gh) is missing. Run the fuzukue install command again in Terminal.'
            return 0
        }
        (unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN GH_CONFIG_DIR
         export GH_PROMPT_DISABLED=1
         probe_cli gh auth status --hostname github.com) && return 1
        say '原稿を読んだり書いたりするだけなら、GitHub へのログインはあとでかまいません。要望やプルリクエストを送るときは、ターミナルで次の行を実行し、表示されたコードを使ってブラウザで自分の GitHub アカウントにログインしてください。' \
            'You can read and write locally before signing in to GitHub. To send requests or pull requests, run this line in Terminal and use the displayed code to sign in with your own GitHub account in the browser.'
        if [ "$gh_command" = "$gh_bin/gh" ]; then
            printf '%s\n' '"$HOME/Library/Application Support/fuzukue/desktop-install/bin/gh" auth login --hostname github.com --git-protocol https --web'
        else
            # The resolved tool may be outside the login shell's PATH.
            quoted_gh=$(printf '%s\n' "$gh_command" | sed "s/'/'\\\\''/g")
            printf "'%s' auth login --hostname github.com --git-protocol https --web\n" "$quoted_gh"
        fi
    }

    require_cli() {
        "$skip_agents" && return 0
        if probe_cli claude --version || probe_cli codex --version; then return 0; fi
        if ask '会話に使う公式ツールを選んでください。［1: Claude Code / 2: Codex / Enter: 終了］' \
            'Choose an official tool for conversations. [1: Claude Code / 2: Codex / Enter: exit]'; then
            case $answer in
                1) url=https://claude.ai/install.sh; shell=/bin/bash; provider=claude ;;
                2) url=https://chatgpt.com/codex/install.sh; shell=/bin/sh; provider=codex ;;
                *) provider= ;;
            esac
            if [ -n "$provider" ] &&
                { if [ -n "$fixture" ]; then cp "$fixture/$provider.sh" "$work/provider.sh"; else fetch "$url" "$work/provider.sh"; fi; } &&
                (PATH="$bundle/Contents/Resources/bin:$cli_path" "$shell" "$work/provider.sh" </dev/tty) &&
                probe_cli "$provider" --version; then
                return 0
            fi
        fi
        fail '会話には Claude Code か Codex が必要です。ターミナルで同じインストールコマンドをもう一度実行してください。' \
             'Claude Code or Codex is required for conversations. Run the same install command again in Terminal.'
    }

    if [ "$mode" = guide ]; then
        saved_path=
        if [ -f "$base/cli-path" ]; then IFS= read -r saved_path < "$base/cli-path" || :; fi
        cli_path_for "$saved_path"
        message=
        for hint in login_hint git_hint gh_hint; do
            if guidance=$("$hint"); then
                if [ -n "$message" ]; then
                    message=$(printf '%s\n\n%s' "$message" "$guidance")
                else
                    message=$guidance
                fi
            fi
        done
        [ -n "$message" ] || exit 0
        if "$stage_only"; then printf '%s\n' "$message"; exit 0; fi
        osascript - "$message" >/dev/null <<'APPLESCRIPT'
on run argv
    display dialog (item 1 of argv) with title "fuzukue" buttons {"OK"} default button "OK" giving up after 300
end run
APPLESCRIPT
        exit $?
    fi

    [ ! -L "$base" ] && [ ! -L "$app" ] && [ ! -L "$server_unit" ] && [ ! -L "$update_unit" ] || local_error
    if [ -e "$app" ] && [ "$(plist CFBundleIdentifier "$app/Contents/Info.plist")" != com.fuzukue.desktop ] &&
        { [ ! -f "$base/release" ] || [ ! -f "$support_script" ]; }; then
        fail '同じ名前のアプリがすでにあります。ホームの「アプリケーション」でそのアプリを移動してください。' \
             'An app with that name already exists. Move it out of Applications in your home folder.'
    fi
    for unit in "$server_unit" "$update_unit"; do
        if [ -e "$unit" ] && ! owned_unit "$unit"; then local_error; fi
    done
    if ! "$stage_only"; then
        launchctl print "$domain" >/dev/null 2>&1 ||
            fail 'Mac の画面にログインし、ターミナルを開いてやり直してください。' \
                 'Log in to your Mac desktop, open Terminal, and try again.'
        for label in "$server_label" "$update_label"; do
            if loaded "$label" && ! owned_job "$label" "$agents/$label.plist"; then occupied_error; fi
        done
    fi
    # Scheduled package replacement needs a server-side admission reservation.
    if [ "$mode" = update ]; then exit 0; fi
    if [ "$mode" = uninstall ] && [ ! -d "$base" ]; then exit 0; fi

    work= app_stage= transaction= lock_owned=false
    rollback=false server_was_loaded=false server_started=false update_started=false
    server_stopped=false app_moved=false new_app=false
    backup() { if [ -f "$1" ]; then cp -p "$1" "$2"; fi; }
    restore() {
        if [ -f "$1" ]; then mv -f "$1" "$2"; else rm -f "$2"; fi
    }
    cleanup() {
        recovery_failed=false
        if "$rollback"; then
            if "$update_started"; then stop_job "$update_label" || :; fi
            if "$server_started"; then stop_job "$server_label" || :; fi
            if "$new_app"; then rm -rf "$app" || recovery_failed=true; fi
            if "$app_moved"; then
                { [ ! -e "$app" ] && mv "$app_stage/previous.app" "$app"; } || recovery_failed=true
            fi
            restore "$transaction/old-secretaire" "$base/secretaire" || recovery_failed=true
            restore "$transaction/old-release" "$base/release" || recovery_failed=true
            restore "$transaction/old-cli-path" "$base/cli-path" || recovery_failed=true
            restore "$transaction/old-server.plist" "$server_unit" || recovery_failed=true
            restore "$transaction/old-update.plist" "$update_unit" || recovery_failed=true
            if "$server_stopped" && "$server_was_loaded"; then
                launchctl bootstrap "$domain" "$server_unit" >/dev/null 2>&1 || :
            fi
        fi
        if [ -n "$work" ]; then rm -rf "$work"; fi
        if ! "$recovery_failed"; then
            if [ -n "$app_stage" ]; then rm -rf "$app_stage"; fi
            if [ -n "$transaction" ]; then rm -rf "$transaction"; fi
        fi
        if "$lock_owned"; then rm -f "$base/install.lock"; fi
    }
    trap cleanup 0
    trap 'exit 1' HUP INT TERM

    mkdir -p "$base" || local_error
    if ! shlock -p "$$" -f "$base/install.lock" 2>/dev/null; then
        fail 'インストールか更新が進行中です。少し待ってからやり直してください。' \
             'An install or update is in progress. Wait a moment and try again.'
    fi
    lock_owned=true

    case $mode in
        uninstall)
            if ! "$stage_only"; then
                for label in "$update_label" "$server_label"; do
                    if loaded "$label"; then stop_job "$label" || local_error; fi
                done
            fi
            rm -f "$server_unit" "$update_unit" "$base/secretaire" "$base/release" "$base/cli-path" "$base/auto-update-off" ||
                local_error
            rm -rf "$app" || local_error
            say 'fuzukue を削除しました。作業フォルダー、履歴、設定は残っています。' \
                'fuzukue was removed. Your project, history, and settings are still on this Mac.'
            exit 0 ;;
        no-auto-update|resume-auto-update)
            [ -f "$base/release" ] || local_error
            if [ "$mode" = no-auto-update ]; then
                touch "$base/auto-update-off" || local_error
                say '自動更新を停止しました。' 'Automatic updates are paused.'
            else
                rm -f "$base/auto-update-off" || local_error
                say '自動更新の設定を保存しました。現在、サーバーは手動で更新できます。' \
                    'The update preference was saved. Server updates currently require a manual install.'
            fi
            exit 0 ;;
    esac
    if [ "$mode" = install ]; then
        say 'お使いの Mac に合う fuzukue を探しています。' 'Finding fuzukue for your Mac.'
    fi
    work=$(mktemp -d "${TMPDIR:-/tmp}/fuzukue-install.XXXXXXXX") || local_error
    select_release
    download_packages
    unpack

    if ! "$stage_only"; then
        if loaded "$server_label"; then
            server_was_loaded=true
        elif loopback self-update/health >/dev/null; then
            occupied_error
        fi
    fi
    [ ! -L "$project" ] && [ ! -L "$applications" ] || local_error
    mkdir -p "$project" "$applications" "$agents" "$logs" || local_error
    app_stage=$(mktemp -d "$applications/.fuzukue.XXXXXXXX") || local_error
    transaction=$(mktemp -d "$base/.transaction.XXXXXXXX") || local_error

    saved_path=
    if [ -f "$base/cli-path" ]; then IFS= read -r saved_path < "$base/cli-path" || :; fi
    if [ "$mode" = install ]; then
        cli_path_for "$incoming_path:$saved_path"
        require_git
        require_gh
        require_cli
        setup_project_git || exit 1
    else
        cli_path_for "$saved_path"
    fi
    printf '%s\n' "$cli_path" > "$transaction/cli-path" || local_error
    printf '%s\n' "$tag" > "$transaction/release" || local_error
    mv "$bundle" "$app_stage/fuzukue.app" && mv "$work/secretaire" "$transaction/secretaire" || local_error
    write_server_unit "$transaction/server.plist" >/dev/null 2>&1 || local_error
    write_update_unit "$transaction/update.plist" >/dev/null 2>&1 || local_error
    backup "$base/secretaire" "$transaction/old-secretaire" &&
        backup "$base/release" "$transaction/old-release" &&
        backup "$base/cli-path" "$transaction/old-cli-path" &&
        backup "$server_unit" "$transaction/old-server.plist" &&
        backup "$update_unit" "$transaction/old-update.plist" || local_error

    # Stage both downloads before changing the running service. Roll back the pair if startup fails.
    rollback=true
    if "$server_was_loaded"; then
        stop_job "$server_label" || local_error
        server_stopped=true
    fi
    if [ -e "$app" ]; then
        mv "$app" "$app_stage/previous.app" || local_error
        app_moved=true
    fi
    mv "$app_stage/fuzukue.app" "$app" || local_error
    new_app=true
    mv -f "$transaction/secretaire" "$base/secretaire" &&
        mv -f "$transaction/server.plist" "$server_unit" &&
        mv -f "$transaction/cli-path" "$base/cli-path" || local_error
    if ! "$stage_only"; then
        launchctl bootstrap "$domain" "$server_unit" >/dev/null 2>&1 || local_error
        server_started=true
        wait_healthy ||
            fail 'fuzukue を起動できませんでした。Mac を再起動してから、同じインストールコマンドをもう一度実行してください。' \
                 'fuzukue could not start. Restart your Mac and run the same install command again.'
    fi
    mv -f "$transaction/update.plist" "$update_unit" || local_error
    if ! "$stage_only" && ! loaded "$update_label"; then
        launchctl bootstrap "$domain" "$update_unit" >/dev/null 2>&1 || local_error
        update_started=true
    fi
    mv -f "$transaction/release" "$base/release" || local_error
    if [ "$mode" = install ]; then
        "$app/Contents/MacOS/fuzukue" --configure-external >/dev/null 2>&1 ||
            fail 'アプリの接続設定を保存できませんでした。fuzukue を閉じてから、もう一度実行してください。' \
                 'The app’s connection settings could not be saved. Quit fuzukue and try again.'
    fi
    rollback=false

    if [ "$mode" = install ]; then
        bundle=$app
        say 'インストールできました。ホームの「アプリケーション」にある fuzukue を開けます。' \
            'fuzukue is installed. Open it from Applications in your home folder.'
        if ! "$skip_agents"; then login_hint || :; fi
        gh_hint || :
        if [ -z "$fixture" ]; then
            open "$app" >/dev/null 2>&1 ||
                say 'Finder の「アプリケーション」から fuzukue を開いてください。' \
                    'Open fuzukue from Applications in your home folder using Finder.'
        fi
    else
        say "fuzukue を $tag に更新しました。" "fuzukue was updated to $tag."
    fi
)

{ install_fuzukue "$@"; }
