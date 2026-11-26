# リリースからインストールする

対象は Linux / WSL2 x86_64 のサーバーと、Windows / Linux x86_64 のデスクトップクライアントです。macOS のサーバーとクライアントには [Mac のベータ版ガイド](install-mac.md) があります。公開済みの対応リリースが必要です。スクリプトはこのリポジトリの信頼できるチェックアウトか、同じリリースタグの公式ソースアーカイブから実行します。POSIX クライアント用のスクリプトは、同じディレクトリの `install-server.sh` を使います。

既定の `beta` の初回選択は OS によって異なります。macOS は対応するプレリリースを選びます。Linux / WSL2 は安定版を優先し、Windows はプレリリースを優先します。Windows / Linux の更新では両方が対象です。 `FUZUKUE_CHANNEL=stable` は安定版だけが対象です。 `FUZUKUE_VERSION` に `v` から始まる公開タグを指定すると、その版を選べます。現在より古い版を選ぶ場合もタグを指定します。ドラフトは取得対象に含まれません。

インストーラーは HTTPS で GitHub Releases からファイルを取得し、タグが指すコミットをマニフェストと照合します。パッケージのサイズ、SHA-256、`SHA256SUMS` の一致を確認してから実行ファイルを展開します。マニフェストへの署名は今後の対応です。ダウンロードとチェックサムの取得には、同じ配布元を信頼する必要があります。

## Linux / WSL2

Bash からでも `sh` からでも実行できます。Python 3.8 以降、curl、CA 証明書と GNU coreutils が必要です。Linux クライアントにはグラフィカルセッションと、Wayland または X11、XKB、fontconfig、Vulkan の対応ランタイムが必要です。CI は Ubuntu 24.04 を対象とし、他のディストリビューションでの互換性は未確認です。

```sh
sh scripts/install-server.sh
FUZUKUE_CHANNEL=stable sh scripts/install-client.sh
~/.local/bin/secretaire /path/to/project
~/.local/bin/fuzukue
```

サーバーの既定の接続先は `ws://127.0.0.1:7878/ws` です。Windows 上で使う場合は、WSL2 内でサーバーを起動し、Windows 側でクライアントを起動します。

サーバーは `${XDG_DATA_HOME:-$HOME/.local/share}/fuzukue/<tag>/` に入ります。クライアントの版は同じルートの `client/` に入ります。各 `current` と `previous` は実行ファイルを指し、`~/.local/bin` のランチャーが引数と作業ディレクトリを引き継ぎます。PATH に `~/.local/bin` があれば名前だけで起動できます。更新の前に、手動で起動したサーバーやクライアントを終了します。

```sh
FUZUKUE_VERSION=v0.2.0-beta.1 sh scripts/install-server.sh --dry-run
sh scripts/install-server.sh --rollback
sh scripts/install-server.sh --uninstall
sh scripts/install-client.sh --uninstall
```

`--dry-run` は取得と検証だけを行います。 `--rollback` は現在版と直前の版を入れ替えます。アンインストールは管理下のランチャーと実行ファイルを削除し、設定・状態・プロジェクトと旧ソースビルドを保持します。

### エージェント CLI の導入と確認

macOS と Linux / WSL2 のサーバーインストーラーは `claude` と `codex` を探し、各 CLI が報告した版とログイン状態を表示します。足りない CLI は表示された公式コマンドを個別に承認すると導入できます。明示的にまとめて承認するには `--install-agents`（`--yes`）を使い、確認と導入を省くには `--skip-agents` を使います。導入後は自分で `claude auth login` または `codex login` を実行し、`sh scripts/install-server.sh --check-agents` で状態を再確認します。サーバーの起動に CLI は不要で、会話にはログイン済みの CLI が一つあれば足ります。Windows クライアントを使う場合は、その接続先のサーバー（例として WSL2）へ CLI を入れます。 [Claude Code](https://code.claude.com/docs/en/setup) · [Codex CLI](https://github.com/openai/codex#quickstart)

### 既存サーバーからの移行

既存サーバーと同じユーザー、プロジェクト、XDG 環境を使い、稼働中の旧サーバーを停止します。旧ソース用ユーザーサービスを使っている場合は、そのサービスを停止・無効化してから新しいサービスに切り替えます。初回インストール時に `${XDG_STATE_HOME:-$HOME/.local/state}/fuzukue/server-builds/` があれば、`current` と `previous` が指す `<commit>/build-*/secretaire` を新ルートにコピーします。旧 `current` が最初のロールバック先になり、旧ディレクトリとリンクもそのまま残ります。旧版が現在の状態を読めることを確認してからロールバックします。

```sh
sh scripts/install-server.sh --systemd /path/to/project
systemctl --user status fuzukue-release.service
```

`--systemd` はユーザーサービスを有効にして起動します。WSL2 では systemd を有効にしたディストリビューションで使います。既定の起動確認はループバックの 7878 番です。サーバーの待ち受けを変更している場合は `--health-port` で合わせます。起動確認に失敗したときは切り替え前のリンクとサービスを復元する処理が走ります。サービスの実機確認は未実施です。Linux のジョブごとのメモリ上限と RAM に応じた既定値は [サービスガイド](service.md) を参照してください。設定・起動引数も同ガイドにあります。

## Windows

Windows x86_64 の Windows PowerShell 5.1 と `curl.exe` が必要です。既存クライアントを終了してから実行します。

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-client.ps1 -Shortcut
& "$env:LOCALAPPDATA\fuzukue\install\client\fuzukue.cmd"
```

実行ポリシーの指定は起動した PowerShell プロセスに適用されます。 `-Shortcut` は「fuzukue Release」をスタートメニューに追加します。 `-Channel stable` や `-Version v0.2.0` でチャネルや版を指定できます。既存の `%APPDATA%\fuzukue\settings.toml` を優先し、未設定の場合は既知のソースビルド先にあるポータブル設定をコピーします。別の場所からコピーするには `-SettingsFrom` を指定します。旧 `%APPDATA%\labo-gui` からの移行はクライアントが初回起動時に行います。

配布済みの自分のサーバーから導入するには `-Server https://server.example` を指定します。スクリプトは `/client/windows-x86_64/manifest` を読み、同じ SHA-256 を指定して `/client/windows-x86_64` から実行ファイルを取得します。サーバーの TLS 証明書を信頼できることが必要です。Bearer 認証を使うサーバーでは、実行前にプロセス環境の `FUZUKUE_INSTALL_TOKEN` に既存の接続トークンを設定します。

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-client.ps1 -Uninstall
```

アンインストール後も設定・データは残ります。Windows 実機での対話インストールは未確認です。

## 開発版とクライアントの更新

[ソースからのビルド](../README.md#quick-start-from-source)と[既存のサーバー経由のクライアント更新](update.md)は引き続き使えます。サーバーからの更新はランチャーが起動した 書き込み可能なクライアント実行ファイルを既存の方式で入れ替えます。公開配布の Linux サーバーはインストーラーの再実行で切り替えます。macOS のベータ版は LaunchAgent を使うと自動で確認・適用します。手動操作も [Mac のベータ版ガイド](install-mac.md) にあります。

## English

Run `sh scripts/install-server.sh` on Linux x86_64 or WSL2, and `sh scripts/install-client.sh` for Linux desktop. On Windows x86_64, use `powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-client.ps1` from a trusted checkout. Use `FUZUKUE_CHANNEL=stable` for stable releases and `FUZUKUE_VERSION` to pin a published tag. Downloads are checked against release metadata and SHA-256. The manifest is unsigned. For a server and desktop client on the same Mac, see the [macOS beta quick start (Japanese)](install-mac.md). Uninstall with `--uninstall` or `-Uninstall` to keep your data and settings.
