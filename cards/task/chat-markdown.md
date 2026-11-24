---
id: chat-markdown
kind: task
title: チャットの本文を Markdown として描く
status: done
links:
  - {to: agent-co-op, rel: see}
files: [crates/fuzukue/src/main.rs, crates/fuzukue-view/src/lib.rs]
---
今はチャットの各行を素の文字で出しているので、エージェントの返答のコードブロック（```）や `code` がそのまま見える。
カード詳細と同じく `TextView::markdown` で描く。ストリーミング中は文字が増えるたびに作り直すので、重さを確かめる。
TextView の折り返し幅の癖（[[inline-links]] に書いたもの）にも気をつける。

2026-11-19 に対応した。右ペインの幅の固定値も外した（右端が切れて見えたのはウィンドウ全体の描画の問題だった。[[windows-right-edge]]）。
