# テストの実行

## Rust のテスト

Rust と Cargo、Git、Python 3、POSIX シェル、C/C++ のビルド環境が必要です。Git と Python 3 はテストのフィクスチャでも使います。Linux のデスクトップ版をリンクするには、環境に応じて XCB、xkbcommon、xkbcommon-x11 の開発用ライブラリも用意してください。

リポジトリのルートから、サーバーと共有クレートのワークスペース、別ワークスペースのデスクトップ版を順に実行します。

```sh
cargo test --workspace --no-fail-fast -- --show-output
cargo test --manifest-path crates/fuzukue/Cargo.toml --workspace --no-fail-fast -- --show-output
```

サービスのシェルから実行する場合は、テスト用のシェルで `FUZUKUE_SERVICE`、`FUZUKUE_SELF_UPDATE_READY`、`FUZUKUE_SELF_UPDATE_SOURCE` と、設定されていれば対応する旧 `LABO_*` 変数を解除します。再起動後の再開を検証するテストは、これらの変数を継承しても、該当する変数を外した子プロセスで検証を続けます。

## 一時ディレクトリ

`TMPDIR` には Git リポジトリの外にある、書き込み可能な既存ディレクトリを指定します。特に `TMPDIR` をリポジトリ内の `target/` に向けると、フィクスチャが親の Git 除外規則や Cargo ワークスペースを拾うことがあります。secretaire の共通フィクスチャは、`TMPDIR` がリポジトリ内を指す場合、最も外側のリポジトリと同じ階層に一時ディレクトリを作ります。その場所に書き込めない場合は、リポジトリ外の `TMPDIR` を用意します。Windows の一時ディレクトリの設定には `TMP` と `TEMP` を使います。

## PDF とループバックの前提

- PDF を実際に生成するテストでは、対象エンジン（pdfLaTeX、LuaLaTeX、upLaTeX）の実行ファイルと対応する `.fmt` が必要です。TeX Live などでエンジン、文書クラス、パッケージ、フォントを用意します。テストによって `latexmk`、`dvipdfmx`、`synctex`、Poppler の `pdfinfo` と `pdftotext`、LuaTeX-ja、EPS 変換用の Ghostscript（`gs`）を使います。
- ローカル HTTP を使うテストでは、`127.0.0.1` の空きポートで TCP リスナーを開ける必要があります。
- テストが必要な実行ファイルや TeX 形式ファイルの不足、またはループバックの利用制限を検出した場合、`SKIPPED: <テスト名>: <理由>` を出力して終了します。`--show-output` で表示できます。実行時のスキップは Rust の集計では `passed` に含まれます。前提確認後のビルド、通信、アサーションの失敗はテストの失敗として報告されます。

`ignored` と表示されるテストは、個別のツールチェーン、外部フィクスチャ、長時間実行などを必要とします。対象の `#[ignore = "..."]` にある前提を用意し、対象名を指定して `-- --ignored --show-output` を付けます。テストが内部で起動する子プロセス用のエントリもあるため、必要な対象だけを選びます。画面の撮影手順は [Linux で Workbench の画面を確認する](headless.md) を参照してください。
