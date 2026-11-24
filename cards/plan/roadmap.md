---
id: roadmap
kind: plan
title: fuzukue 開発ロードマップ
status: accepted
links:
  - {to: card-model, rel: requires}
  - {to: artifact-views, rel: see}
  - {to: agent-backends, rel: see}
  - {to: issue-queue, rel: see}
  - {to: usage-awareness, rel: see}
---

各作業の前提、状態、完了条件、確認した根拠は [ロードマップデッキ](../../roadmap/README.md) に記録する。

## これまでの計画（2026-11-19 時点）

1. カードハーネスの核（`fuzukue-cards`）、サーバーへの組み込み（変更の見張り）、左のカード一覧と右の詳細 — 済（2026-11-19）。
   本文のリンク（[[inline-links]]）も済。
2. 右ペインに PDF（[[artifact-views]]）— 済（2026-11-19）。PDF からカードへ（[[pdf-to-card]]）も済。残り: カードから PDF の該当箇所へ、[[windows-right-edge]]。
3. Claude の部分出力でストリーミング表示 — 済（2026-11-19）。
4. `codex app-server` の取り込みと MCP 公開（[[agent-backends]]、[[agent-co-op]]）— 済（2026-11-19）。
5. 承認要求の UI — 済（2026-11-19、拒否を実地で確認）。
6. 使用量の集計と表示、休憩と自動再開（[[usage-awareness]]）— 済（2026-11-19）。
7. GitHub の作業場（[[issue-queue]]）— 2026-11-19 に Issue の表示、ローカルの作業者（Issue ごとの worktree）、手順書まで。Issue #1 で作業者→監督役の検証→取り込みを一周した。残り: dots に `docs/workshop.md` を常設の指示として渡す（ユーザーが dots 側で設定する）。
8. 会話の一覧と切り替え、サーバー再起動をまたぐ継続（タイムラインの記録と `--resume`）— 済（2026-11-19）。

次の候補: 道具を MCP ツールに・未定義語の検査、右の画面を別ウィンドウに切り出す（[[card-files]] の残り）、研究の縦タブ（[[workspace-tabs]]）の改善、本のデッキを macOS で動かす（[[books]]）、ラベル参照のリンク化（[[reading-view]] の残り）、カードから PDF の該当箇所へ、承認待ちの会話を左ペインで目立たせる、取り込んだ Issue の worktree の片づけ。
