---
id: workspace-tabs
kind: design
title: 研究ごとの縦タブと、研究もデッキにする新しい形
status: accepted
links:
  - {to: card-model, rel: refines}
  - {to: books, rel: see}
files: [crates/secretaire/src/workspace.rs, crates/fuzukue-cards/src/lib.rs, examples/lab/]
---
研究・原稿・本を、すべて `deck.toml` を持つカードのデッキにする。研究室の根には `fuzukue.toml` を置く（そこから下の deck.toml を探す）。サンプルは [[file:examples/lab/README.md]]。

- `deck.toml`: `kind`（research / paper / book）、`title`、`summary`、`status`、`next`、`papers`（研究が扱う原稿のデッキ）、
  `outputs`（PDF）、`[external]`（別のデッキの名前空間）、`[macros]`（数式のマクロ）。
- 研究のデッキは、計画を `kind: plan`、問いを `question`、着想を `idea`、まだ示していない主張を `claim` のカードで持つ。
- 縦タブ: 研究のデッキ 1 つにつき 1 枚。どの研究にも属さない本・原稿も 1 枚ずつ。選ぶとそのデッキが開き、会話はタブごと（`tab-<id>`）。
  タブの会話の Claude には、研究の題・デッキ・次・関係する原稿・絡み合う研究を、システムプロンプトに足して渡す。
- 絡み合い: 研究どうしが同じ原稿を扱っている、または `[external]` で互いのデッキ（研究か原稿）を参照している。
  選んだタブから見て絡み合うタブに「↔ 共有 n」を出し、左ペインに一覧する。
- 互換: `deck.toml` がないプロジェクトは、`research/*/plan.md`から研究を読むか、プロジェクト全体を 1 タブにする。
  `fuzukue-cards` は `deck.toml` → `cards.toml` → `harness.toml` の順に設定を探すので、既存の論文ハーネスも読める。

縦タブから複数の研究を切り替える。サンプルの研究は既存のプロジェクトと別に用意する。

確かめた（2026-11-19）: examples/lab の 6 デッキが診断 0。サーバーが研究 3・本 1 のタブと、3 組の絡み合いを出す。クライアントの縦タブのスクリーンショット。
確かめていない: タブを手で切り替えたときの会話とデッキの切り替え、タブの会話での Claude の振る舞い。
