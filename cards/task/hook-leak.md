---
id: hook-leak
kind: task
title: サーバーが起動する Claude にグローバルフックが効く
status: done
links:
  - {to: usage-awareness, rel: see}
files: [crates/secretaire/src/claude.rs]
---
2026-11-19 の動作確認で、最初の返答に usage-limits フックの指示による「A one-turn file read fits easily…」が混ざった。
サーバー用の設定（`--settings` で専用ファイル、または `--setting-sources` の制限）で切り離す。

2026-11-19 に解決: `--setting-sources project,local` でユーザー設定を読まないようにした。あわせて既定のモデルもユーザー設定から来なくなった（モデルの指定はサーバー側で持つ。未実装）。
