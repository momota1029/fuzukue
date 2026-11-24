# fuzukue

English: [README.md](README.md)

fuzukue は、AI エージェントと一緒に数学のノートや原稿を書き、研究を進めるための作業台です。Rust 製のサーバー `secretaire` がプロジェクトを開き、エージェントを動かします。GPUI 製のデスクトップクライアントでは、会話、数式を含むノート、原稿、PDF を一つの作業環境で扱えます。Android クライアントでは、スマートフォンやタブレットから指示を送り、会話を確認し、承認依頼に応答できます。[Android ガイド](android/README.md)

開発初期のプロジェクトです。メンテナーが日々の作業で使いながら開発しており、セットアップや使い方は変わることがあります。

## 主な機能

- 定義、定理、証明、ノートをカードとデッキにまとめます。カード間の参照をたどれます。実例はサンプルのワークスペースで確認できます。[カードグラフ](docs/graph.md) · [サンプルのワークスペース](examples/lab/README.md)
- 証明や原稿のレビューで人やエージェントが見つけた指摘を、デッキごとの指摘台帳で追跡します。[指摘台帳](docs/review-findings.md)
- スレッド単位で、エージェントにサーバー上の特定フォルダーの読み取りを許可できます。許可期間を指定でき、許可の確認や取り消しもできます。[フォルダーの読み取り許可](docs/folder-grants.md)
- 返信の Markdown に埋め込んだ XML 部品を、デスクトップでコールアウト、表、レビュー指摘として表示します。[XML 部品](docs/xml-components.md)
- 研究デッキの探索ツリーを開き、問いや予想を枝ごと、または進捗状況ごとに確認できます。[探索ツリーの例](examples/lab/research/curvature/exploration/tree.json)
- 接続先サーバーからデスクトップ版の更新通知を受け取り、中断したダウンロードを再開できます。[クライアントの配布と更新](docs/update.md)

## リリースからインストールする

対応パッケージは [インストールガイド](docs/install.md) を参照してください。同じ Mac でサーバーとクライアントを試す手順は [Mac のベータ版ガイド](docs/install-mac.md) にあります。

## ソースから起動する

Git と Rust のツールチェーンを用意します。リポジトリのルートでサーバーをビルドし、サンプルのワークスペースを開きます。

```sh
cargo build --locked --release -p secretaire
./target/release/secretaire examples/lab
```

サーバーの既定の待ち受けは `127.0.0.1:7878` です。Linux または macOS では、別のターミナルを開き、リポジトリのルートからデスクトップクライアントをビルドして起動します。GUI のビルドには OS ごとの開発ツールが必要です。macOS でのデスクトップのビルドと動作は検証途上です。[Mac ガイド](docs/mac.md)

```sh
cargo build --locked --release --manifest-path crates/fuzukue/Cargo.toml
FUZUKUE_URL=ws://127.0.0.1:7878/ws ./crates/fuzukue/target/release/fuzukue
```

Linux または macOS では、[`scripts/run.sh`](scripts/run.sh) に `examples/lab` を渡すと、サーバーとクライアントをまとめてビルドし、起動できます。

Windows では、サーバーを WSL2 で、クライアントを Windows で動かします。WSL2 に Ubuntu を用意し、Windows のローカルドライブ上にリポジトリをクローンして、Rust の MSVC ツールチェーンと Visual Studio Build Tools の C++ ワークロードを入れます。Windows 側のクローンから PowerShell でセットアップの内容を確認し、実行します。スクリプトは WSL 側でサーバー、Windows 側でクライアントをビルドし、接続を設定してスタートメニューにショートカットを作成します。[Windows ガイド](docs/windows.md) · [セットアップスクリプト](scripts/setup-windows.ps1)

```powershell
.\scripts\setup-windows.ps1 -WhatIf -Shortcut
.\scripts\setup-windows.ps1 -Shortcut
```

エージェントを使う場合は、`secretaire` を動かす環境に対応する CLI を入れ、自分のアカウントでログインします。

Android のソースビルドと署名済み APK の作成手順は [Android ガイド](android/README.md) を参照してください。リポジトリには [Android のリリーススクリプト](scripts/release-android.sh) があります。

## 自作スキルを追加する

`<workspace>/skills/<name>/SKILL.md` を作り、付属ファイルも同じスキルのフォルダーに置きます。`SKILL.md` の YAML フロントマターには `description` が必要です。役名は `name` で指定でき、省略するとフォルダー名になります。

```markdown
---
description: 原稿の記号を確認する。
---

各記号が最初に使われる前に定義されているか確認する。
```

スキルはそのワークスペースで読み込まれます。委譲時に作業役として選ぶと、本文がワーカーに渡ります。同名の項目がある場合、標準の読み込み順は組み込み、規則パック、個人用拡張、プロジェクトの拡張、ワークスペース直下のスキルで、後の項目が優先されます。

Claude ワーカーではユーザー設定と Claude Code 固有のスキル読み込みが無効なので、`~/.claude/skills` のスキルはワークスペースの `skills/` や拡張機能に移します。`SKILL.md` 内に Claude Code や Codex 固有のツール名やコマンドがあれば、ワーカーが使えるツールに合わせて書き換えます。

複数のワークスペースで使う、または他の人と共有する場合は、`extension.toml` と `skills/` を含む拡張機能にします。個人用なら `~/.config/fuzukue/extensions.toml`、プロジェクト用なら `fuzukue.toml` に登録し、ローカルパスまたは Git の取得元を指定します。設定手順やその他の使い方は[拡張機能ガイド](docs/extensions.md)を参照してください。

## 別の端末から接続する

ローカルでは、デスクトップクライアントから `ws://127.0.0.1:7878/ws` にループバックで接続できます。Tailscale は任意です。

リモート接続には Tailscale を推奨します。サーバーとクライアントを同じ tailnet に参加させ、サーバーが自身の Tailscale IP で待ち受けるように設定し、クライアントの Tailscale ログインを許可します。クライアントのリモート接続設定には、サーバーの Tailscale IP を使います。

tailnet 外の端末は、HTTPS リバースプロキシと端末ごとのトークンで接続できます。両方のリモート接続の設定は [リモート接続ガイド](docs/remote.md) を参照してください。

## 更新

サーバーの運用者は `secretaire` にデスクトップ版のビルドを登録できます。サーバーが接続中のデスクトップクライアントに新版を通知し、クライアントはダウンロードして再起動すると更新できます。ダウンロードが中断した場合は、受信済みの続きから再開できます。更新前に、ファイルサイズと SHA-256 を検証します。[デスクトップ版の更新ガイド](docs/update.md)

Android 版は署名済み APK を使い、アプリ内から更新を確認できます。[Android 版の更新ガイド](android/README.md#配布と更新)

## 報告・質問・貢献

利用者やコントリビューターからの不具合報告、提案、質問は [GitHub Issues](https://github.com/momota1029/fuzukue/issues) で受け付けます。メンテナーの開発計画は [ロードマップのデッキ](roadmap/README.md) で管理します。Issue やプルリクエストを開く前に、[貢献ガイド](CONTRIBUTING.ja.md) と [AI と一緒に PR を送る手順](docs/contributing-with-ai.md) を参照してください。

## セキュリティ

このリポジトリでは、GitHub の非公開の脆弱性報告が有効です。GitHub の [非公開の報告フォーム](https://github.com/momota1029/fuzukue/security/advisories/new) から報告してください。

## ライセンス

fuzukue の自作コードは `MIT OR Apache-2.0` で公開します。[MIT License](LICENSE-MIT) または [Apache License 2.0](LICENSE-APACHE) のどちらかを選べます。同梱する第三者のコードや素材には、それぞれのライセンスが適用されます。[第三者ライセンス](THIRD_PARTY_NOTICES)
