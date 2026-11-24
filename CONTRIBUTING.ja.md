# fuzukue への貢献

English: [CONTRIBUTING.md](CONTRIBUTING.md)

不具合報告、提案、質問、プルリクエストを歓迎します。

## 開発状況

fuzukue は開発初期で、メンテナーが日常的に使いながら改善しています。返事に時間がかかることがあります。

## Issue を開く

[GitHub Issues](https://github.com/momota1029/fuzukue/issues) は、利用者やコントリビューターからの報告、提案、質問に応じる窓口です。メンテナー自身の作業計画は [ロードマップのデッキ](roadmap/README.md) で管理します。

- 不具合は、起きたこと、期待した動作、再現手順、バージョンまたはビルド ID、OS、使用したクライアントやサーバーを書いてください。
- 提案は、どんな場面で何をしたいか、今どこで困っているかを書き、試した方法があれば添えてください。
- 質問は、知りたいことと、必要に応じて読んだガイドや試した手順を書いてください。

公開する本文、ログ、画像には、トークンや認証情報、tailnet の名前やアドレス、個人のファイルパス、非公開文書の内容を含めないでください。

## セキュリティ報告

脆弱性の詳細や再現手順は、GitHub の [非公開の報告フォーム](https://github.com/momota1029/fuzukue/security/advisories/new) からメンテナーに伝えてください。

## プルリクエスト

小さな修正は直接プルリクエストを送ってください。機能追加や大きな変更は、先に Issue を開いて目的と進め方を相談してください。プルリクエストには変更の理由と、実行したテストや確認結果を書いてください。

## AI を使った貢献

AI の支援を使った貢献も歓迎します。プルリクエストに AI を使った箇所や用途を書き、変更内容を自分で確認して、関連するテストを実行してください。AI ツールに機密情報を入力したり、生成された差分に機密情報を含めたりしないでください。

## テスト

変更に関係するコマンドを実行してください。環境の準備は [README](README.ja.md) を参照してください。

リポジトリのルートで Rust ワークスペース（サーバーと共通クレート）をテストします。デスクトップは別のマニフェストを使います。

```sh
cargo test --locked --workspace
cargo test --locked --manifest-path crates/fuzukue/Cargo.toml
```

Linux でデスクトップの画面を確認する場合は、[ヘッドレス検証ガイド](docs/headless.md) の Xvfb の準備手順を参照してください。次のコマンドでチャット画面のシナリオを実行できます。

```sh
scripts/shot.sh examples/lab tests/shots/chat.json
```

Android のプロトコル互換性と同梱ライセンスを確認する場合は、リポジトリのルートで次を実行します。ライセンスの確認には JDK 21 を使います。ユニットテストの手順は [Android ガイド](android/README.md) にあります。

```sh
scripts/check-android-protocol.sh
scripts/check-android-notices.sh
```

## ドキュメント

使い始める手順は [README](README.ja.md)、設定や機能ごとのガイドは [`docs/`](docs/) にあります。

## ライセンス

貢献は、プロジェクトの自作部分と同じ `MIT OR Apache-2.0` の条件でライセンスされます。ライセンス本文は [MIT License](LICENSE-MIT) と [Apache License 2.0](LICENSE-APACHE) を参照してください。
