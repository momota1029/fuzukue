# fuzukue

fuzukue は、AI エージェントと一緒に研究資料、メモ、原稿を扱うためのデスクトップアプリです。プロジェクトのファイルを扱うサーバーが `secretaire`、接続してカードや PDF を読み、エージェントと作業するクライアントが `fuzukue` です。

開発初期のソース公開です。作者自身の環境で日常的に試しながら開発しており、設定や使い方は変わることがあります。Android ベータは今後の予定です。

## ソースから起動する

Git と Rust のビルド環境を用意し、リポジトリのルートから実行します。まずサンプルを開けます。

```sh
cargo build --locked --release -p secretaire
./target/release/secretaire examples/lab
```

サーバーの既定の待ち受けは `127.0.0.1:7878` です。Linux または macOS のデスクトップでは、別のターミナルで次を実行します。GUI のビルドには OS の開発ツールが必要です。macOS でのデスクトップのビルドと動作は検証途上です。

```sh
cargo build --locked --release --manifest-path crates/fuzukue/Cargo.toml
FUZUKUE_URL=ws://127.0.0.1:7878/ws ./crates/fuzukue/target/release/fuzukue
```

Windows では WSL2 の Ubuntu でサーバーを動かし、Windows 側でクライアントをビルドします。Windows のローカルドライブ上にクローンし、Rust の MSVC ツールチェーンと Visual Studio Build Tools の C++ ワークロードを用意してから、PowerShell で実行します。

```powershell
.\scripts\setup-windows.ps1 -WhatIf -Shortcut
.\scripts\setup-windows.ps1 -Shortcut
```

詳しい準備は [Windows ガイド](docs/windows.md) と [Mac ガイド](docs/mac.md) を参照してください。エージェントを使う場合は、サーバーを動かす環境で対応する CLI をインストールし、自分のアカウントで認証します。

## 別の端末から接続する

リモート接続には Tailscale を推奨します。サーバーとクライアントを同じ tailnet に参加させ、サーバーの設定で Tailscale のアドレスと許可するログインを指定します。クライアントの「設定 → サーバー → リモート」でそのアドレスを使い、接続します。

同じマシンで使う場合はループバックで接続できるので、Tailscale は不要です。tailnet 外から接続する場合は HTTPS リバースプロキシと端末トークンを使います。設定例は [リモート接続ガイド](docs/remote.md) にあります。

## ライセンス

fuzukue の自作部分は [MIT License](LICENSE-MIT) または [Apache License 2.0](LICENSE-APACHE) のいずれかを選んで利用できる形 で公開します。同梱する第三者のコードや素材には各コンポーネントのライセンスが適用されます。同梱物のライセンスは [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) に記載しています。

## English

fuzukue is an early, dogfooded desktop workbench for research files, notes, PDFs, and AI agents. Build and start the `secretaire` server, then run the `fuzukue` desktop client with the commands above. On Windows, the PowerShell setup builds the server under WSL2 and the client on Windows. Tailscale is recommended for remote access and optional for local use. Outside a tailnet, use an HTTPS proxy and a device token. An Android beta is planned.
