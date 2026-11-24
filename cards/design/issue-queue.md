---
id: issue-queue
kind: design
title: GitHub を作業場にする（Issue を、dots やローカルのエージェントが拾う）
status: accepted
links:
  - {to: context-selection, rel: requires}
  - {to: card-model, rel: requires}
files: [docs/workshop.md, CONTRIBUTING.md, crates/secretaire/src/github.rs]
---
- Issue の本文に入口カード（`cards: [id, ...]`）と受け入れ条件を書く。ラベル `dot:ready` が「拾ってよい」の印。
- dots（OpenAI の常時稼働エージェント）は定期実行で `dot:ready` の古いものから拾い、
  `work/issue-<N>` ブランチに GitHub アプリ経由で成果と `notes/heartbeat/` の心拍を公開し、Issue にコメントする。
  dots は CLI の `git push` ができず、ファイル単位の公開になる（yulang での試し、2026-11-08）。
- 監督役（Claude か Codex）が検証して取り込む。書き込みの最終判断は監督役が持つ（yulang の研究ハーネスと同じ原則）。
- `secretaire` が Issue とブランチを見張り、左ペインに状態（ready / running / stale / done）を出す。
- Issue とカードは相互に指す（カードの `issues`、Issue の `cards`）。

ユーザー（2026-11-19）: 「GitHubに問題を載せておいて，dotに解決させるような仕組みがあるととてもよい．今dotは使い放題ですから」

決定（2026-11-19、ユーザー）: この提案について「正直それでいいと思います」

ユーザー（2026-11-19）:「dotsというかGitHub作業場って感じだけどね」

実装（2026-11-19）:
- 作業場の手順は [[file:docs/workshop.md]]。fuzukue リポジトリの公開 Issue の運用は [[file:CONTRIBUTING.md]] を参照。
- secretaire は `gh` で開いている Issue と `work/issue-<N>` ブランチを 2 分ごと（とクライアントの要求時）に読み、配る。
- fuzukue の左ペインに Issue、右ペインに本文・入口カード・ブランチの状態と、「ローカルで取り組む」「dot:ready にする」ボタン。
- 「ローカルで取り組む」は、入口カードから `fuzukue-cards context` で読むべきカードを並べた依頼文を作り、セッション `issue-<N>` の Claude に渡す。
