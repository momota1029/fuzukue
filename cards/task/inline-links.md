---
id: inline-links
kind: task
title: 本文の [[id]] をクライアントでリンクとして描く
status: done
links:
  - {to: card-model, rel: implements}
files: [crates/fuzukue/src/main.rs, crates/fuzukue-view/src/lib.rs]
---
右ペインの本文は `TextView::markdown` で描いており、`[[agent-backends]]` のような本文中のリンクが文字のまま出る。
Markdown に渡す前に `[[id]]` をリンクへ書き換え、クリックでそのカードを選ぶ。`[[file:...]]` はファイルを開く（ビューができたら）。

あわせて: `TextView` は親の padding を差し引かずに折り返すため、今は幅を固定値（476px）で与えている。右ペインの幅を可変にするときに直す。

2026-11-19 に対応した。右ペインは固定幅を解除した。右端の欠けはウィンドウ全体に及ぶ描画の問題だった（[[windows-right-edge]]）。
