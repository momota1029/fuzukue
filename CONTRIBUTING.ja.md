# fuzukue への貢献

English: [CONTRIBUTING.md](CONTRIBUTING.md)

不具合報告、提案、質問、プルリクエストを歓迎します。

## 開発状況

fuzukue は開発初期で、メンテナーが日常的に使いながら改善しています。返事に時間がかかることがあります。

## Issue を開く

[GitHub の Issue フォーム](https://github.com/momota1029/fuzukue/issues/new/choose) は、利用者やコントリビューターからの報告、提案、質問に応じる窓口です。メンテナー自身の作業計画は [ロードマップのデッキ](roadmap/README.md) で管理します。

- 不具合は、起きたこと、期待した動作、再現手順、バージョンまたはビルド SHA、OS、使用したクライアントやサーバーを書いてください。
- 提案は、やりたいこと、現行版で起きたこと、試した回避策を書いてください。
- AI に変更を頼んだ場合は「AI に変更を頼んだ」フォームに、依頼内容、結果、途中で困ったことを書いてください。
- 質問は、知りたいことと、必要に応じて読んだガイドや試した手順を書いてください。

公開する本文、ログ、画像には、トークンや認証情報、非公開ネットワークの名前やアドレス、個人のファイルパスやメールアドレス、非公開資料の内容を含めないでください。

## 公開 Issue の整理

メンテナーが必要に応じて使うラベルです。

| ラベル | 用途 |
| --- | --- |
| `bug` | 期待と異なる動作 |
| `enhancement` | 改善の提案 |
| `question` | 使い方や設定の質問 |
| `needs-info` | 再現手順やビルド情報の追加が必要 |
| `duplicate` | リンク先の Issue やカードで追跡済み |

## セキュリティ報告

脆弱性の詳細や再現手順は、GitHub の [非公開の報告フォーム](https://github.com/momota1029/fuzukue/security/advisories/new) からメンテナーに伝えてください。

## プルリクエスト

小さな修正は自分の fork の作業ブランチから `momota1029/fuzukue:main` 宛てにプルリクエストを送ってください。機能追加や大きな変更は、先に Issue を開いて目的と進め方を相談してください。[PR テンプレート](.github/PULL_REQUEST_TEMPLATE.md) を記入し、関連 Issue や[ロードマップカード](roadmap/README.md)があればリンクを添えてください。

## AI を使った貢献

AI の支援を使った貢献も歓迎します。プルリクエストに AI を使った箇所や用途を書き、変更内容を自分で確認して、関連するテストを実行してください。AI ツールに機密情報を入力したり、生成された差分に機密情報を含めたりしないでください。fork の準備から公開までの流れは [AI と一緒に PR を送る手順](docs/contributing-with-ai.md) を参照してください。

## テスト

変更に関係するコマンドを実行してください。環境の準備は [README](README.ja.md) を参照してください。

push の前にクリーンな作業ツリーで `scripts/pre-pr-check.sh` を実行すると、ブランチの履歴の検査、Rust ワークスペースのテスト、Cards のチェックをまとめて実行できます。`TMPDIR` は Git リポジトリの外にある書き込み可能な場所を指定してください。Rust のテストを個別に実行する場合はリポジトリのルートで次を使います。デスクトップは別のマニフェストを使います。

```sh
unset FUZUKUE_SERVICE FUZUKUE_SELF_UPDATE_READY FUZUKUE_SELF_UPDATE_SOURCE
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

CI は Linux の Rust テストと macOS のサーバーのコンパイル確認を実行します。Cards は専用のワークフローで確認します。Android の JVM テストは SDK の準備と Gradle のダウンロードに時間がかかるため、変更に応じて手動で実行します。

## ドキュメント

使い始める手順は [README](README.ja.md)、設定や機能ごとのガイドは [`docs/`](docs/) にあります。

## ライセンス

貢献は、プロジェクトの自作部分と同じ `MIT OR Apache-2.0` の条件でライセンスされます。ライセンス本文は [MIT License](LICENSE-MIT) と [Apache License 2.0](LICENSE-APACHE) を参照してください。
