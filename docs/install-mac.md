# Mac で fuzukue を使う

Apple Silicon と Intel の Mac に対応します。macOS 15（Sequoia）以降で「ターミナル」を開き、次の一行を貼り付けて Return を押します。

```sh
curl -fsSL https://raw.githubusercontent.com/momota1029/fuzukue/main/install.sh | sh
```

このコマンドは、Apple Silicon・Intel 用のアプリとサーバーがそろったリリースが公開されてから使えます。公開前に試す場合は、下の[クローンからセットアップする](#クローンからセットアップする)手順を使います。

ホームの「アプリケーション」（`~/Applications`）にアイコン付きの fuzukue が入ります。次回からは Finder で fuzukue をダブルクリックします。Mac にログインするとバックグラウンドでサーバーが起動します。最初の作業フォルダーは `~/fuzukue-project` です。

1. **Git を用意する。** Git が使えない場合は、ターミナルで `y` を入力して Return を押します。macOS の画面が開くので「インストール」を選びます。インストールにはしばらくかかります。Apple の画面が完了したら、ターミナルで上の一行をもう一度実行します。Git がすでに使える Mac ではこの手順を省けます。 [Apple の案内](https://developer.apple.com/documentation/xcode/installing-the-command-line-tools)
2. **GitHub CLI（gh）を用意する。** 使える `gh` があれば、そのまま使います。なければ `y` と Return で公式版 2.105.0 をホームに導入します。インストーラーは Mac の種類に合う ZIP の SHA-256 を照合し、`gh` をホームの Application Support に置きます。この手順に `sudo` や Homebrew は不要です。 [GitHub CLI の配布ファイルと SHA-256](https://github.com/cli/cli/releases/expanded_assets/v2.105.0)
3. **会話用ツールを用意する。** Claude Code と Codex の両方がなければ、どちらかの公式インストーラーを選びます。会話を始めるには、案内に表示されるコマンドをターミナルで実行し、自分のアカウントでログインします。その後、fuzukue を開き直します。
4. **変更履歴に表示する名前を決める。** Git に名前の設定がなければ、ニックネームなどを入力します。メールアドレスの設定がない場合は、この作業フォルダーだけに配達不能な `fuzukue@users.noreply.invalid` を設定します。既存の Git の名前やメール設定は引き続き使います。初回は空のコミットを作り、もともとフォルダーにあったファイルや準備済みの変更はそのままにします。

### GitHub へのログインはあとでできる

原稿の編集、会話、ローカルの変更履歴には GitHub へのログインは不要です。GitHub CLI を使って GitHub の要望やプルリクエストを扱うときに、ターミナルの案内からログインの行をコピーします。すでに自分で入れた `gh` がターミナルで使える場合は、次の行でも始められます。

```sh
gh auth login --hostname github.com --git-protocol https --web
```

fuzukue のインストーラーで `gh` を入れた場合は、次の行を使います。

```sh
"$HOME/Library/Application Support/fuzukue/desktop-install/bin/gh" auth login --hostname github.com --git-protocol https --web
```

表示された一時コードを使い、開いたブラウザで自分の GitHub アカウントにログインして GitHub CLI を承認します。別の人のアカウントがブラウザに表示された場合は、自分のアカウントに切り替えます。 [GitHub CLI のログイン手順](https://cli.github.com/manual/gh_auth_login)

### 変更履歴で戻せる範囲

fuzukue の「変更履歴」は、エディタでの保存、記録対象のエージェント作業、サーバー稼働中に検知した外部編集などを、専用のローカル Git 履歴に記録します。差分を開き、対象の変更を取り消せます。既定の保持期間は 90 日で、件数の上限は 2,000 件です。Git の除外設定や生成物の除外も適用されます。

作業フォルダーの通常の Git 履歴には、初期コミットと、明示的な操作やエージェントからの Git 操作で作成したコミットが入ります。保存のたびに通常の Git コミットを作る機能はありません。コミットしていない変更は、記録済みの fuzukue の変更履歴か、作業中のファイルに残ります。エディタで未保存の内容や、サーバー停止中または監視の合間に上書きされ、どちらの履歴にも記録されなかった中間版は履歴から取り戻せません。

### 更新と削除

Mac のサーバーとアプリを更新するには、作業を終えてから上の一行をもう一度実行します。パッケージの自動適用は停止しています。このインストーラーで入れた `gh` は、再実行時に新しい公式版が用意されていれば更新するか尋ねます。もともと入っていた `gh` は、その導入方法で更新します。

自動更新の停止設定を保存するには次を実行します。`--resume-auto-update` はこの設定を解除します。

```sh
sh "$HOME/Applications/fuzukue.app/Contents/Resources/install.sh" --no-auto-update
```

削除するときは fuzukue を閉じて、ターミナルで次を実行します。アプリ、サーバー、起動と更新の登録を削除し、作業フォルダー、履歴、設定、会話用ツール、GitHub CLI は保持します。

```sh
sh "$HOME/Applications/fuzukue.app/Contents/Resources/install.sh" --uninstall
```

配布版の照合には、macOS 標準の `curl`（HTTPS）、`osascript` の JavaScript for Automation（JSON）、`shasum -a 256`（SHA-256）、`stat`（サイズ）、`tar`（展開）、`codesign`（アドホック署名）を使います。`plutil` でアプリと自動起動の設定を確認し、`launchctl` と `open` で起動します。GitHub 上のタグが指すコミット、配布マニフェスト、サイズと SHA-256 を照合します。ベータのマニフェストは未署名で、アプリは Apple の公証を受けていません。[macOS が起動を止めた場合の Apple の案内](https://support.apple.com/ja-jp/102445)

## クローンからセットアップする

Mac にログインし、「ターミナル」を開きます。初めて Git を使うときに macOS が開発ツールの導入を求めたら、Apple の案内に従うか、`xcode-select --install` を実行します。

```sh
cd "$HOME"
git clone https://github.com/momota1029/fuzukue.git
cd fuzukue
sh scripts/setup-unix.sh --dry-run
sh scripts/setup-unix.sh
"$HOME/.local/bin/fuzukue"
```

- プレビューは macOS のバージョンと CPU を表示し、必要なツールを確認します。Apple Silicon では Rosetta を使わないターミナルで実行します。
- このクローンのデスクトップビルドには、macOS SDK と C/C++ コンパイラを含む **Command Line Tools**、Python 3.8 以降、[リポジトリが指定する Rust ツールチェーン](../rust-toolchain.toml)が必要です。GPUI Kit の macOS バックエンドは、Rust 1.98.0 で安定化された `Option::map_or_default` を使います。GPUI Kit は `runtime_shaders` を有効にしており、アプリの起動時に macOS の Metal API でシェーダーをコンパイルします。フル版 Xcode と追加の Metal Toolchain のダウンロードは不要です。[Apple の Command Line Tools ガイド](https://developer.apple.com/documentation/xcode/installing-the-command-line-tools)
- セットアップは不足しているツールの導入コマンドを表示します。公式インストーラーを開く、または実行する場合は対話端末で確認します。Rust と Cargo の依存関係の取得にはインターネット接続が必要です。初回のビルドには時間がかかります。
- 既定ではサンプル `examples/lab` を開きます。手元のプロジェクトを使う場合は `sh scripts/setup-unix.sh --project "$HOME/my-project"` のようにホームディレクトリ内の既存フォルダーを渡します。
- サーバーはログインユーザーの LaunchAgent から `127.0.0.1:7878` で起動します。セットアップはクライアントの接続モードを「外部」に設定します。クライアントから同じ Mac のサーバーに接続する際にトークンは不要です。クライアントを開いたまま更新する場合は、次回起動時に新しい実行ファイルが使われます。
- Claude Code と Codex CLI の有無とログイン状態が表示されます。会話を試すには、自分の Mac で `claude auth login` または `codex login` を実行します。状態を再確認するには `sh scripts/install-server.sh --check-agents` を実行します。CLI を確認せずにセットアップするには `--skip-agents` を使います。

クローンから更新するときは、クライアントを閉じて `git pull --ff-only` を実行し、同じ `--project` の指定でセットアップを再実行します。サーバーの起動確認が通ってからクライアントのランチャーを切り替えます。
サーバーのログは `~/Library/Logs/fuzukue/` にあり、LaunchAgent の状態と停止方法は [配布版の接続手順](#3-起動して接続する) と共通です。削除するときはクローンで `sh scripts/install-client.sh --uninstall` と `sh scripts/install-server.sh --uninstall` を実行します。
サーバーのソース自己更新によるサービスの切り替えは Linux の組み込み systemd サービス用です。Mac の自動更新を使うには、以下の手順で公開された配布版へ移行します。

Mac には Linux の Bubblewrap による作業役・レシピのホスト隔離がありません。CLI の通常の確認を使い、最初は専用フォルダーで短い会話とファイルの表示を試します。

## 配布版へ切り替える

クローンから導入済みで作業フォルダーとコマンドラインランチャーを使い続ける場合は、以下の手順で案内されたベータ版へ切り替えられます。新しい一行インストーラーに移る場合は、先に既存のサーバーを停止します。

### 1. 準備する

- 「ターミナル」で `uname -m` と `sw_vers -productVersion` を実行します。Apple Silicon ではネイティブのターミナルを使い、`arm64` と表示されることを確かめます。Intel Mac では `x86_64` です。
- `python3 --version` で Python 3.8 以降を確認します。入っていなければ [Python の macOS インストーラー](https://www.python.org/downloads/macos/)を使えます。ベータ版の実行に Rust や Xcode は不要です。Git などを使うときに macOS から求められた場合は `xcode-select --install` で Command Line Tools を入れます。
- 会話を試すには、Claude Code か Codex CLI のどちらか一つに、自分の Mac の同じユーザーでログインします。サーバーのインストール時に CLI の有無と版、ログインの有無が表示されます。両方がなくても、サーバーの起動とファイルの表示を試せます。

### 2. インストールする

クローンがある場合はクローンのルートから実行します。配布版から始める場合は [GitHub Releases](https://github.com/momota1029/fuzukue/releases) から案内されたプリリリースの **Source code (zip)** を取得し、展開したフォルダーでターミナルを開きます。以下の `v0.2.0-beta.1` は例です。案内されたタグに置き換え、サーバーとクライアントに同じタグを使います。

```sh
export FUZUKUE_CHANNEL=beta FUZUKUE_VERSION=v0.2.0-beta.1
sh scripts/install-server.sh --dry-run
sh scripts/install-client.sh --dry-run
sh scripts/install-server.sh
sh scripts/install-client.sh
export PATH="$HOME/.local/bin:$PATH"
secretaire --version
fuzukue --version
```

サーバーのインストーラーは、現在の PATH と `~/.local/bin`、Homebrew などの一般的な場所で `claude` と `codex` を探します。各 CLI 自身の読み取り専用の状態確認を使い、版とログイン状態だけを表示します。CLI がなければ公式インストーラーの実行コマンドを表示し、対話端末で `y` と答えたものだけを実行します。 `--yes` または `--install-agents` はこの承認をまとめて与えます。確認を含めて省略するには `--skip-agents` を使います。

ログイン操作は本人が行います。

- Claude Code は `claude auth login` を実行し、自分の Claude アカウントでブラウザーの案内に従います。必要なら `claude` 内の `/login` を使います。 [Claude Code の公式セットアップ](https://code.claude.com/docs/en/setup)
- Codex CLI は `codex login` を実行し、自分の ChatGPT アカウントでログインします。 [Codex CLI の公式セットアップ](https://github.com/openai/codex#quickstart)

導入やログインの再確認は、サーバーの再起動をせずに `sh scripts/install-server.sh --check-agents` で行えます。サーバーが古い PATH で起動していて新しく追加したディレクトリが必要な場合は、CLI が見えるターミナルから LaunchAgent の登録コマンドを再実行します。

`--dry-run` は配布物を取得して照合します。インストーラーは GitHub のタグが指すコミット、マニフェスト、ファイルサイズ、SHA-256 を確認します。今のベータ版ではマニフェストへの署名と Apple の公証がまだありません。配布元と案内されたタグを確かめてから実行します。

ランチャーは `~/.local/bin`、版ごとの実行ファイルは `~/Library/Application Support/fuzukue/install/server` と `client` に入ります。 `--version` はランチャーが選んだタグを表示します。`export PATH=…` を新しいターミナルでも使うなら `~/.zshrc` に入れます。フルパスの `"$HOME/.local/bin/secretaire"` と `"$HOME/.local/bin/fuzukue"` でも起動できます。

#### 「開発元を検証できない」と表示されたとき

このベータ版はアドホック署名で配布するため、ダウンロード方法や macOS の設定によって Gatekeeper が起動を止めることがあります。起動を一度試した後、「システム設定」→「プライバシーとセキュリティ」の「このまま開く」が表示される場合は、配布元を確認したうえで選びます。 [Apple の案内](https://support.apple.com/ja-jp/102445)

ターミナルから起動する実行ファイルに隔離属性が付いていた場合は、配布元とハッシュを確認済みのファイルに限って、その属性を取り除けます。次の例はクライアント用です。サーバーの場合は `client` を `server` に、末尾の `fuzukue` を `secretaire` に変えます。

```sh
root="$HOME/Library/Application Support/fuzukue/install/client"
exe="$root/$(readlink "$root/current")"
xattr -p com.apple.quarantine "$exe"
# 上で隔離属性が確認でき、信頼できる配布物だと確認した場合だけ実行
xattr -d com.apple.quarantine "$exe"
```

### 3. 起動して接続する

ターミナルで試用フォルダーを作り、ログイン中の自分のユーザーに LaunchAgent を登録します。LaunchAgent は登録時のターミナルの PATH と一般的な CLI の保存先を使います。独自の場所に CLI や Node.js がある場合は、その場所を先に PATH に通します。作業履歴など Git を使う機能を試す場合は `git init "$HOME/fuzukue-dogfood"` も実行します。

```sh
mkdir -p "$HOME/fuzukue-dogfood"
sh scripts/install-server.sh --launchd "$HOME/fuzukue-dogfood"
```

サーバーを確認して、クライアントを開きます。

```sh
curl -fsS http://127.0.0.1:7878/self-update/health
FUZUKUE_URL=ws://127.0.0.1:7878/ws "$HOME/.local/bin/fuzukue"
```

ヘルスチェックは数字の PID を返します。クライアントの「設定」→「サーバー」は既定の「外部」を使います。同じ Mac のループバック接続では fuzukue の接続トークンは不要です。クライアントに作業フォルダーが表示されたら、会話欄から自分がログインしたバックエンドを選び、短いメッセージを送ります。クライアントを閉じても LaunchAgent は動き続けます。

既存の PDF の表示も試せます。原稿のビルドや逆引きまで試す場合は MacTeX の `latexmk`・`synctex` と、Poppler の `pdfinfo`・`pdftotext` を必要に応じて用意します。前面起動か LaunchAgent の PATH から実行できることを確かめます。 [ソース版の Mac ガイド](mac.md)

ログは `~/Library/Logs/fuzukue/` に出ます。状態は `launchctl print "gui/$(id -u)/com.fuzukue.secretaire"` で確認できます。一時停止には `launchctl bootout "gui/$(id -u)/com.fuzukue.secretaire"` を使います。次回ログイン時には再び起動します。ログインし直さず起動するには、同じインストールコマンドを再実行します。

#### ターミナルで直接起動する

LaunchAgent の登録や CLI の認証で困った場合は、登録済みのサーバーを一時停止してから次を実行します。この起動方法ではインストーラーを手動で再実行して更新します。サーバーを止めるにはターミナルで Ctrl-C を押します。

```sh
FUZUKUE_ADDR=127.0.0.1:7878 "$HOME/.local/bin/secretaire" "$HOME/fuzukue-dogfood"
```

### 4. 更新する・戻す・外す

LaunchAgent で使う場合、自動更新は既定でオンです。ログイン時、起動時とおおむね 1 時間ごとに、公開 GitHub Releases の beta をトークンなしで確認します。初回に指定した `FUZUKUE_VERSION` は初回導入のタグです。更新は今より新しく、両方のパッケージがそろった版を選びます。

作業中はサーバーを切り替えず、次の確認を待ちます。更新時の停止ではサーバーが実行中の処理を終了できるように待機します。新版の起動確認に失敗した場合は直前の版に復旧する処理が走ります。クライアントは切り替わったサーバーと同じ版を次回起動時から使います。動作中の画面はそのまま使えますが、サーバーの更新後に再接続できない場合はクライアントも開き直します。選択中の版はそれぞれ `--version` で確認できます。

両方の自動更新を止めるか再開するには、信頼できるソースを展開したフォルダーで次を実行します。

```sh
sh scripts/install-server.sh --auto-updates off
sh scripts/install-server.sh --auto-updates on
```

手動で更新するときはクライアントを閉じ、手動起動中のサーバーを止めます。新版のソースを展開し、新しいタグを `FUZUKUE_VERSION` に設定して両インストーラーを再実行します。問題が出た場合は、旧版が現在のデータを読めることを確認してから、更新された側を `--rollback` で戻します。

```sh
sh scripts/install-server.sh --rollback
sh scripts/install-client.sh --rollback
```

利用を終えるときはクライアントと手動起動中のサーバーを停止し、LaunchAgent を登録した同じユーザーのターミナルから外します。

```sh
sh scripts/install-client.sh --uninstall
sh scripts/install-server.sh --uninstall
```

登録と実行ファイルを外し、設定、サーバーの状態、ログ、作業フォルダーを残します。設定は通常 `~/Library/Application Support/fuzukue/`、サーバーの状態は `${XDG_STATE_HOME:-$HOME/.local/state}/fuzukue/` にあります。

### 5. 困ったとき

案内を受けた相手に、macOS のバージョン、`uname -m`、両ランチャーの `--version`、止まった手順、表示されたエラーを伝えます。LaunchAgent なら `secretaire.err.log`、手動起動ならターミナルの末尾も添えられます。送る前にトークン、会話や原稿の内容、環境固有のパスを取り除きます。画面が開く場合は、PDF 表示、会話、再起動のうちどこまで通ったかも記録します。
