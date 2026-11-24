---
id: task-model
kind: design
title: 裏の連絡路（専用ブランチ）で仕事を回し、Issue と同じ画面で扱う
status: proposed
links:
  - {to: issue-queue, rel: refines}
  - {to: issue-threads, rel: see}
  - {to: research-workshop, rel: see}
  - {to: handoff, rel: see}
  - {to: standards-audit, rel: see}
---

> dotが独自の仕組みで動いてるのですが、issuesに変えるのは骨が折れそうです
> いや、確かに悪くない仕組みだとは思う……。ただエディタがやるにはちょっと汚染源になりがちだから、もうちょっといい感じに設計し直してissuesと統合した仕組みにするのがよさそう
> 秘密の通信路なわけじゃん。あんまり表を通さずにやれないかなーって。
> Bは悪くない。Gitが使えればhubじゃなくても使えるし、確実だから。都度消してもいいしね

## 問題

Yulang の dots は、リポジトリのファイルで仕事を回している: 計画表（`notes/plan/m1-plan.md` の節ごとの状態）、取り合い防止の「状態だけのコミット」、`work/<ID>` のブランチ、報告（`notes/handoff/*-results.md`、`dots-cycle-<N>.md`）、ハートビート、質問（`notes/questions/`）、監督役の指示（`dots-inbox.md`、ACTIVE/STOP）。

仕組みとしては筋がよいが、エディタ（fuzukue）がこれを読み書きすると、帳簿の更新がプロジェクトの履歴に混ざる（汚染源）。研究の成果物と、仕事の管理の記録が同じ場所に積もり、差分・査読・取り込みの邪魔になる。リポジトリごとに書式も違う。

## 方針（2026-11-21、利用者が B を選んだ）

- dots と監督役（fuzukue）のやり取りは裏の連絡路。表（本体のブランチの履歴、公開の Issue）には流さない。
- 連絡路は、同じリポジトリの **専用ブランチ** `fuzukue/ops`。本体と履歴を共有しない孤立したブランチ（orphan）にする。Git が使えれば GitHub 以外でも動き、dots は今のやり方（GitHub アプリでブランチにファイルを書く）でそのまま書ける。
- fuzukue は利用者の作業ツリーを触らない。専用ブランチの読み書きは、別の索引（`GIT_INDEX_FILE`）と `commit-tree` で行い、チェックアウトしない。本体のブランチには一切書かない。
- 履歴は都度消してよい: 「たたむ」で今の中身だけを持つ新しい根のコミットに置き換える（`--force-with-lease`）。古い報告は消える。本体の履歴には影響しない。
- 画面では、連絡路の仕事と GitHub の Issue を同じ形（状態、依存、担当、報告、スレッドとの紐づけ）で扱う。公開したい仕事だけ Issue に写せる。

## 既にあるもの

- 孤立したブランチに別の中身を置く慣習（`gh-pages` など）。
- git-bug: Issue を Git のオブジェクトと参照に保存し、ホストに依存しない。
- git notes（`refs/notes/*`）。画面に出ない代わりに、ホストや道具の対応が弱いので今回は使わない。
- 書式は、カードと同じ「YAML の前書き＋ Markdown」。

## 連絡路の中身

```
fuzukue/ops（孤立したブランチ）
  tasks/<ID>.md            仕事。前書き: id, title, state(todo|running|needs-verify|needs-owner|done), deps, owner, claimed_at, branch, issue（写した Issue があれば）。本文: 目標、受け入れの条件、手がかり。
  orders/<agent>.md        常設の指示。前書き: order_id, status(ACTIVE|STOP), updated。本文が今の指示。書くのは利用者と監督役だけ。
  heartbeat/<agent>.md     生存確認。cycle, status, started, updated, activity, resume_hint, report。作業者が毎回書き換える。
  reports/<agent>/<N>.md   サイクルの報告。
  reports/tasks/<ID>/<日時>.md  仕事の報告（やったこと、証明した定理と読める主張、確かめたこと、確かめていないこと、残り）。
  questions/<qid>.md       質問。前書き: blocks: [T1c], status(open|answered)。選択肢は遅延する案を先頭に。答えは利用者が書き足す。
```

- 書き手を分ける: 作業者は自分の heartbeat・reports・questions と、取った仕事の `tasks/<ID>.md` の状態だけを書く。指示と仕事の追加は利用者・監督役。ファイルが分かれているので、押し合いになっても読み直して積み直せば済む。
- 取り合いの防止: `tasks/<ID>.md` を `running` と担当に書き換えたコミットを、早送りでだけ押す。弾かれたら取り直し、他の担当がいれば諦める（GitHub アプリの内容の更新も、元のファイルの版を指定するので同じ働き）。
- 成果物（Lean、コード）は今までどおり `work/<ID>` のブランチ。取り込みは [[branch-review]]（Lean は主張の確認）。

## fuzukue がすること

- 研究 Issue の画面に、連絡路の仕事と GitHub の Issue を並べる: 状態、依存、担当、最新の報告、質問、作業者ごとの常設の指示と生存状況（updated から「止まった疑い」）。
- [[issue-threads]] と同じく、仕事をスレッドに紐づけ、連絡路の変化（報告、状態、質問、止まった疑い）をカードとして流す。見張りは `git fetch` で、ブランチの先頭が変わった時だけ読む。
- 指示の書き換え、仕事の追加・差し戻し・done は、差分を見せて確かめてから一つのコミットにする。
- 「たたむ」（履歴の圧縮）、「Issue に写す」「Issue から取り込む」。
- 移行の道具: 今の計画表（`notes/plan/m1-plan.md`）・指示・質問・最新のハートビートを読んで、連絡路の初期の中身の案を作り、確かめてから専用ブランチに置く。本体のファイルは変えない（古い計画表をしまうかは利用者が決める）。

## dots の側

- 常設の指示を「`fuzukue/ops` の `orders/dots-main.md` と `tasks/` を読み、`heartbeat/`・`reports/`・`questions/` に書く」に書き換える（指示の文面は利用者が決める）。
- 書き換えるまでの間は、fuzukue が今のファイル（`work/dots-main` の報告とハートビート、計画表、質問）を読み取りだけで表示する。そちらには書かない。

## 作業役の並行実行（第 2 段の設計）

- 受付は委譲ごとに基点のコミットを記録し、専用のブランチと Git worktree を作る。
- 作業役はその worktree で編集とテストを行い、差分と検証結果を受付に返す。
- 受付は実際の差分を確認し、取り込みを直列化する。競合や失敗があれば成果を worktree に残し、解消後に再検証する。
- 統合先に利用者の未コミットの変更があれば保護し、取り込み後に残す必要がなくなった worktree を片づける。
- Cargo の依存取得キャッシュとコンパイラの成果物キャッシュを共有し、同時にビルドする worktree の可変な target は分ける。
- ビルドの同時実行数と target の保存容量を管理し、検証に必要なキャッシュを残しながら古い生成物を掃除する。
