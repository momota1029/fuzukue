---
kind: handoff
from: Claude Code session fixture
transcript: /fixtures/transcripts/fixture.jsonl
repo: /fixtures/project
date: 2026-01-01
---

# サンプル原稿の参照を整える

## 残っている仕事

### 1. 補題への参照を照合する

- 内容: 例題のカード ID を照合する。
- 受け入れ条件: `fuzukue-cards check` が通る。

### 2. 原稿の PDF を確認する

- 内容: サンプル原稿を再ビルドする。

## 動いている作業者

| スレッド ID | 担当 | worktree / ブランチ | 状態 | 期待する成果 |
|---|---|---|---|---|
| `00000000-0000-4000-8000-000000000001` | 定義の確認 | `/home/example/lab-definitions` / `work/definitions` | 作業中 | 参照一覧 |

## リポジトリの状態

- 統合用のブランチ: main。
