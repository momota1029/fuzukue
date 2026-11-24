---
id: appearance
kind: design
title: クライアントの外観と読みやすさ
status: accepted
links:
  - {to: artifact-views, rel: refines}
  - {to: books, rel: refines}
files: [crates/fuzukue/src/appearance.rs, crates/fuzukue-view/src/appearance.rs]
---

テーマ、フォント、表示の読みやすさを設定できるようにする。

- 左の「設定」か、Ctrl/⌘+K の「外観設定」から変更する。テーマはライト・ダーク・OS 追従（既定）。ソースの構文色、数式、チャットの統計、PDF ビューの背景もテーマに合わせる。
- UI・本文・ソースのフォントを個別に選ぶ。Windows の既定は Yu Gothic UI・Yu Mincho・Cascadia Mono。未インストールなら順に代替候補を探し、最後は OS の UI フォントを使う。
- UI と本文の文字サイズ、本文の行の高さ、数式の拡大率をすぐに反映する。Ctrl/⌘ と `=` / `-` でも本文の文字サイズを変えられる。
- 設定はクライアントの設定ディレクトリ（Windows は `%APPDATA%\fuzukue\settings.toml`）に保存する。撮影用の `FUZUKUE_THEME=dark` / `FUZUKUE_THEME=light` は起動中だけテーマを固定する。
