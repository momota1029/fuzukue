# fuzukue への貢献

不具合報告、提案、質問、プルリクエストを歓迎します。

Bug reports, suggestions, questions, and pull requests are welcome.

## 開発状況

fuzukue は開発初期で、メンテナーが日常的に使いながら改善しています。返事に時間がかかることがあります。

The project is early and dogfooded. Replies may take time.

## Issue を開く

[GitHub Issues](https://github.com/momota1029/fuzukue/issues) は、利用者やコントリビューターからの報告、提案、質問に応じる窓口です。メンテナー自身の作業計画は [`roadmap/` のカード](roadmap/README.md)で管理し、Issues には転記しません。

- 不具合は、起きたこと、期待した動作、再現手順、バージョンまたはビルド ID、OS、使用したクライアントやサーバーを書いてください。
- 提案は、どんな場面で何をしたいかから書き、試した方法があれば添えてください。
- 質問は、知りたいことと、必要に応じて読んだガイドや試した手順を書いてください。

公開する本文、ログ、画像には、トークンや認証情報、tailnet の名前やアドレス、個人のファイルパス、非公開文書の内容を含めないでください。

Issues are for requests from users and contributors. Describe your goal, environment, or steps as relevant, and keep sensitive information out. Maintainers plan in the roadmap deck.

## セキュリティ報告

脆弱性の悪用方法、再現手順などの詳細は公開 Issue に書かないでください。「質問・相談」フォームから、題名にも本文にも詳細を含めず、非公開の報告窓口を尋ねる Issue を開いてください。

Use the Question form to ask for a private reporting channel. Keep exploit details out of the public issue.

## プルリクエスト

小さな修正は直接プルリクエストを送ってください。機能追加や大きな変更は、先に Issue を開いて目的と進め方を相談してください。プルリクエストには変更の理由と、実行したテストや確認結果を書いてください。

Send small fixes directly. Open an issue first for larger changes and describe the tests you ran in the PR.

## AI を使った貢献

AI の支援を使った貢献も歓迎します。プルリクエストに AI を使った箇所や用途を書き、変更内容を自分で確認して、関連するテストを実行してください。AI ツールに機密情報を入力したり、生成された差分に機密情報を含めたりしないでください。

AI assistance is welcome. Disclose how you used it, review and test the changes, and keep secrets out.

## テスト

変更に関係するコマンドを実行してください。環境の準備は [README](README.md) を参照してください。

Run the checks for the parts you changed. See the README for setup.

リポジトリのルートで Rust ワークスペース（サーバーと共通クレート）をテストします。デスクトップは別のマニフェストを使います。

```sh
cargo test --locked --workspace
cargo test --locked --manifest-path crates/fuzukue/Cargo.toml
```

Linux でデスクトップの画面を確認する場合は、Xvfb を使う [ヘッドレス検証ガイド](docs/headless.md)を参照してください。次はチャット画面のシナリオです。

```sh
scripts/shot.sh examples/lab tests/shots/chat.json
```

Android のプロトコル互換性と同梱ライセンスを確認する場合は、リポジトリのルートで次を実行します。ライセンスの確認には JDK 21 を使います。ユニットテストの手順は [Android ガイド](android/README.md)にあります。

```sh
scripts/check-android-protocol.sh
scripts/check-android-notices.sh
```

## ドキュメント

使い始める手順は [README](README.md)、設定や機能ごとのガイドは [`docs/`](docs/) にあります。

Start with the README. Setup and feature guides are in `docs/`.

## ライセンス

貢献は、プロジェクトの自作部分と同じ `MIT OR Apache-2.0` の条件でライセンスされます。ライセンス本文は [MIT](LICENSE-MIT) と [Apache 2.0](LICENSE-APACHE) を参照してください。

Contributions are licensed under the project's terms, `MIT OR Apache-2.0`.
