---
id: research-workshop
kind: design
title: 研究の問題は研究全体のリポジトリの Issue に置き、作業者に振り分ける
status: accepted
links:
  - {to: issue-queue, rel: refines}
  - {to: agent-backends, rel: see}
  - {to: review-loop, rel: see}
---
ユーザー（2026-11-19）:「よく考えたら研究もこれでできるといいんですよね．dotsにissuesとして置いておくというのもいいですし，自分やOpus/Sol/Fable/Astraで解いてしまうという手も全然あります．特にLeanizeなんかはSol/Terraがいい感じだと思います．」

ユーザー（2026-11-19）:「研究全体の場所のほうがよいと思います．根拠はそんなにありませんが，論文と研究は別物ですのでさすがに論文の中というのはよろしくない．」

## 決定

- 研究の問題（未証明の主張、Lean 化、文献の確認など）の Issue は、論文のリポジトリではなく、研究全体のリポジトリに置く。論文と研究は別物として扱う。
- 作業場のリポジトリは、ワークスペースの設定で指定する。

ユーザー（2026-11-19）:「このリポジトリはlab-guiにして使うリポジトリはlabにしましょう．その方が楽だ．移行って大変なので……」

## 実装の決定

- Issue の front matter または YAML コードブロックには `cards: [<workspace-or-repo>:<deck>#<id>, …]` を書く。カードにも front matter の `issues: [lab#12]` を記録する。リンクは両側に置き、Issue の状態の正本は GitHub とする。作業中・確認待ち・未完はサーバーの作業記録と dots のラベルから表示する。
- 研究用 Issue はどのワークスペースからも一覧できる。開始時は新規スレッドに入口デッキを付け、カードグラフから文脈を集めて作業役に渡す。Lean 化は Sol、証明探索は Fable または Opus（利用できなければ Sol）が既定。検証は元の作業者と別のモデルに回す。振り分けは `workspaces.toml` の `[research.workers]` で設定できる。
- 作業は研究ワークスペースの `work/issue-<N>` の worktree で行う。証明の下書きはファイルに置き、承認すると対象カードの Notes に入る。Lean ファイルも承認時に取り込む。Lean 化の作業役は `lake build` と `#print axioms` を実行し、対象宣言の出力を報告する。sorry または標準外の公理を含む結果は未完とする。
- Issue の作成、dots への投入、成果の取り込みと GitHub へのコメント・クローズには利用者の承認を使う。最初のフローは `examples/lab` の Hamming 距離のカードを使い、スタブで確認する。

操作と成果物の形式は [[file:docs/workshop.md]]。
