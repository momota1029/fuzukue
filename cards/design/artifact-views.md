---
id: artifact-views
kind: design
title: 右ペインの生成物ビュー
status: accepted
links:
  - {to: server-client-split, rel: requires}
files: [crates/fuzukue/src/main.rs, crates/secretaire/src/cards.rs, vendor/gpui-pdf/]
---
- PDF: `gpui-pdf` 0.7（hayro による pure Rust 描画、仮想化ページ、目次、拡大縮小）。2026-11-19 に採用し、日本語・数式の PDF が描けることを確かめた。
  `gpui-pre` 0.3.12 に合わせる 1 行の修正を入れて `vendor/gpui-pdf` に写してある（[[file:vendor/gpui-pdf/VENDORED.md]]）。
- PDF はサーバーから HTTP で取り、Windows の一時フォルダに置いて開く。左ペインの「出力」に、プロジェクト内の PDF を並べる。
- synctex でカードと PDF を双方向に結ぶ。対応表はサーバー側で引く。
- サーバーの `FileChanged` で取り直し、`replace_bytes` で入れ替える（スクロール位置と拡大率を保つ）。2026-11-19 に確かめた。
- yulang 用には Lean のゴール、テスト結果、差分のビューを足す（[[domains]]）。
