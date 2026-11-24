---
id: localisation
kind: design
title: 多言語対応
status: accepted
files:
  - crates/fuzukue-i18n
  - crates/fuzukue/src/i18n.rs
  - locales/ja
  - locales/en
---

# 多言語対応

デスクトップの既定言語は日本語。外観設定の「言語」で「システムに合わせる」「日本語」「English」を選べる。選択は同じ `settings.toml` の `locale` に保存する。キーのない設定ファイルも日本語で開く。OS 追従は `sys-locale` で Windows、macOS、Linux の優先言語を調べ、地域付きタグは親の言語も試す。一致する言語がない場合は日本語に戻る。選択を変えると開いている画面を再描画し、入力欄と gpui-kit の表示言語も更新する。

## Rust

`locales/<言語タグ>/*.ftl` に領域ごとの文言を置く。`fuzukue-i18n` がビルド時に全カタログを埋め込む。クライアントの `tr!` はカタログを引くので、描画関数に言語を渡す必要はない。

```rust
tr!("chat-send")
tr!("paper-findings", count = count)
```

キーは画面や操作の意味で付け、文言を直すときも維持する。文の順序や句読点はカタログで指定する。英語で単数と複数が異なる文言には Fluent のセレクターを使い、件数を数値で渡す。要素 ID、ファイル名、モデルの識別子、文書の見出しを読むための文字列は表示言語から独立させる。

プロトコル 22 ではクライアントの `Hello.locale` と `SetLocale` で接続ごとの表示言語を指定する。承認の要約はキーと引数を持ち、サーバーは接続言語で整形済みの要約も送る。v21 以下には日本語の要約を送る。ビルド、集計、査読などのイベントも配信時に表示言語を反映する。内部エラーや外部プログラムの出力は詳細欄に残し、何の処理に失敗したかを表示言語で付ける。再接続や履歴再生で、別の端末が選んだ言語に引きずられない。

プロンプト、MCP の道具の説明、カードや入力内容、テスト用の文章は翻訳対象外。受付のシステムプロンプトだけに、直近の利用者メッセージの言語で、設定された口調に従って返す指示を加える。

gpui-kit 0.10.0 の検索パネルの置換ボタンや、部品既定のダイアログ文言には既存の翻訳を使う。検索パネルの検索欄と置換欄はこの版ではプレースホルダー自体がなく、アプリ側から設定する API もない。OS のファイル選択ダイアログの操作ボタンは OS の表示言語に従う。

## 言語を追加する

1. `locales/<言語タグ>/` を追加し、既存と同じキー、同じ引数名で翻訳する。キー `settings-language-<言語タグ>` にその言語の自称を入れ、ほかの全カタログにも同じキーを足す。設定画面はディレクトリから言語一覧を作る。
2. Android に言語別の `strings.xml` を追加する。英語は `res/values-en/`、例えばブラジル向けポルトガル語なら `res/values-pt-rBR/` を使う。既定の `res/values/strings.xml` は日本語。翻訳には `ui_locale` でサーバーに通知するタグを指定する。
3. `cargo test -p fuzukue-i18n` で、全言語の Fluent 構文、キーと引数の一致、欠落・未使用キー、日本語リテラルの再混入を確認する。撮影でも、新しい言語の画面と GPUI 部品の翻訳を確認する。

## 画面を確認する

```sh
scripts/shot.sh examples/lab tests/shots/i18n-en.json /tmp/lab-shots/i18n-en
scripts/shot.sh examples/lab tests/shots/sidebar.json /tmp/lab-shots/i18n-ja-sidebar
```

シナリオの `{"action":"locale","locale":"en"}` は設定ファイルを編集せず、開いているウィンドウの言語を変更する。最初の接続からサーバー側の文言も英語にするには `connect` の前に置く。

Rust では 2025-05-22 公開の fluent-bundle 0.16.0 と fluent-syntax 0.12.0、2024-11-01 公開の sys-locale 0.3.2 を使う。いずれも公開から 2 週間以上が経ち、上流リポジトリの更新も確認した。[Fluent Rust](https://github.com/projectfluent/fluent-rs) [sys-locale](https://github.com/1Password/sys-locale)
