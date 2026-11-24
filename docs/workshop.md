# GitHub の作業場

fuzukue リポジトリの公開 Issue は利用者やコントリビューターからの依頼を受け付ける。メンテナー自身の作業計画は [`roadmap/`](../roadmap/README.md) に置く。投稿方法は [貢献ガイド](../CONTRIBUTING.md) を参照。

Public issues are for user and contributor requests. Maintainers plan in the roadmap deck.

以下は、GitHub Issue を作業の単位として使うワークスペース向けの手順。作業者は、外の dots（OpenAI の常時稼働エージェント）でも、secretaire が立てるローカルの Claude / Codex でもよい。

## Issue の書き方

- **入口カード**: `cards: <id>, <id>` の行で書く。作業者はここから `fuzukue-cards context <id>...` で読むべきカードを決める。
- **受け入れ条件**: 何ができたら終わりか。確かめ方（コマンド、テスト、スクリーンショット）まで書く。
- **範囲**: 触ってよいファイル・ディレクトリ。書いていなければ、入口カードの `files` とその周辺。

## ラベル

| ラベル | 意味 | 付ける人 |
|---|---|---|
| `dot:ready` | 拾ってよい | ユーザー・監督役 |
| `dot:running` | 誰かが取りかかっている | 作業者 |
| `dot:review` | 成果が `work/issue-<N>` にあり、検証を待っている | 作業者 |

## 作業者の手順

1. `dot:ready` の Issue のうち、番号の小さいものから拾う。拾ったら `dot:ready` を外して `dot:running` を付け、
   Issue に「取りかかる（作業者の名前）」とコメントする。ラベルを変えられない作業者は、コメントだけでよい。
2. `main` から `work/issue-<N>` ブランチを作る。成果はすべてこのブランチに置く。`main` には書かない。
3. 入口カードから読むべきカードを決めて、読む。カードと食い違うことをするなら、先に Issue で質問する。
4. 作業する。触ったファイルとカードのリンクを更新し、`cargo run -q -p fuzukue-cards -- check` と関係するテストを通す。
5. 長い作業では、10 分ごとに `notes/heartbeat/issue-<N>.md` を更新して公開する
   （`status: running|completed`、`started`、`updated`、`activity`、`resume_hint`）。
   次に拾った作業者は、`running` のまま古くなった心拍を見たら、前の作業者が落ちたとみなして続きから再開する。
6. 終わったら Issue にコメントする: やったこと、確かめたこと、確かめていないこと、残り。
   `dot:running` を外して `dot:review` を付ける。
7. 受け入れ条件が曖昧なとき、作業が範囲を超えそうなときは、作業の前に Issue で質問して止まる。

## 監督役（取り込み）

- `dot:review` の Issue のブランチを、手元で検証する（ビルド、テスト、`fuzukue-cards check`、差分を読む）。
- 通れば `main` に取り込んで Issue を閉じる。直しが要れば Issue にコメントして `dot:ready` に戻す。
- 作業者の成果物を、作業者の環境での結果だけで信じない。検証は監督役が自分の手元でやり直す。

## 作業者ごとの制約

- **dots**: CLI の `git push` ができず、GitHub アプリ経由でファイル単位に公開する（yulang での試し、2026-11-08）。
  多数のファイルにまたがる変更は向かない。検証、調査、ドキュメント、小さな実装を回す。
- **ローカルの Claude**: fuzukue の「ローカルで取り組む」で、セッション `issue-<N>` として始まる。git も gh も使える。
- **Codex**: Claude から `codex_run` で呼ばれるか、`/codex` で直接。ブランチの操作は呼んだ側が持つ。

## 研究 Issue

研究 Issue は現在開いているワークスペースの GitHub リポジトリに置く。既定はその Git リポジトリの `origin`。別の取得先を使うときは、ワークスペース直下の `fuzukue.toml` に指定する。サーバーはワークスペースを読み込むときに取得先を決める。

```toml
[research]
repo = "owner/repo"
```

作業役の既定値はサーバーの状態ディレクトリにある `workspaces.toml` の `[research.workers]` で設定する。

```toml
[research.workers]
lean = "gpt-6.2-sol" # gpt-6.1-terra も選べる
# proof = "claude-opus-5-5"
# check = "gpt-6.1-terra"
```

`gh auth status` と `gh repo view owner/repo` で参照権限を確かめる。研究側のローカル Git に `main` が必要。別の原稿リポジトリは「ワークスペースを開く」で登録する。参照の最初の要素は、ワークスペース ID、名前、ディレクトリ名、もしくは GitHub の origin の `owner/repo` または `repo`。候補が重複したら ID で指定する。デッキのルートを指すときは `.` を使う。

`research_issue_create` には `title`、`cards`、`kind`、`acceptance` を渡す。任意の `assignee` は `local`、`dots`、またはモデル名。手書きの Issue は本文の先頭の front matter、または本文中の YAML ブロックで入口を指定する。

```yaml
cards:
  - sample-paper:papers/levy-families#thm-sphere-concentration
  - lab:research/concentration#claim-x
kind: lean
assignee: local
```

ブロックの後に受け入れ条件を書く。研究 Issue の入口と開閉状態は GitHub に置く。作成時と着手時に、対象カードの front matter の `issues` に `lab#12` の形式でリンクを追記する。参照が複数ある場合、順番が下書きの番号になる。`research_issue_list` は `kind`（`lean` / `proof` / `literature` / `check` / `other`）と `state`（`ready` / `running` / `needs_review` / `incomplete` / `done`）で絞れる。

サイドバーの「研究 Issue」で中央の一覧を開き、Issue を選んで「取り組む」を押す。MCP では `research_issue_start {number}`。カードと依存カードを読んで依頼を組み立て、操作元のワークスペースに新規スレッドを作る。Lean 化は Sol、証明探索は利用できる Fable、Opus、Sol の順が既定。検証（`check`）の `producer` に元のモデルを指定できる。省略時は入口カードが重なる過去の作業記録を参照する。元のモデルが不明なら指定が必要。dots は `dot:ready` への投入を承認してから外部作業者が拾う。

作業役は研究ワークスペースと同じ親ディレクトリにできる `lab-issue-<N>` の worktree（`work/issue-<N>`）で作業する。カードごとの Notes や証明の下書きは `notes/workshop/issue-<N>/1.md`、`2.md` … に書く。Lean ソースは `lean/` に置く。Lean 化は `lake build` を実行して成功時に `lake build: ok` を報告し、対象宣言ごとの `#print axioms` の出力も残す。カードの `lean` に指定された全宣言を対象とする。未指定なら受け入れ条件に対象を明記する。標準の三公理は `propext`、`Classical.choice`、`Quot.sound`。公理の報告が不足するか、`sorryAx` や標準外の公理があると「未完」になる。dots は同じ成果物と `notes/workshop/issue-<N>/summary.md` をリモートの作業ブランチに置き、`dot:review` を付ける。成果を取り込むにはローカルの `origin` を研究リポジトリに合わせる。

「確認待ち」で差分、スレッドの報告、Lean の出力を読み、「取り込む」または `research_issue_accept {number}` で承認を求める。承認後、下書きは対象カードの `## Notes` に追記し、変更された `lean/` 配下の `.lean` ファイルは研究側の作業ツリーへ反映する。カード本文の変更や Lean ファイルの削除は手動で行う。反映できたら GitHub にコメントを付けて閉じる。カードと Lean のローカル変更は、必要に応じて各リポジトリでコミットする。Issue の作成と取り込みは `FUZUKUE_APPROVALS` の設定にかかわらず利用者の承認を待つ。

## スレッドで GitHub の更新に応対する

別のワークスペースからの起票と、自動でスレッドを立てる設定は [fuzukue への Issue の起票と受付](issue-intake.md) を参照。

スレッド上部の「Issue に紐づける」から Issue や PR を選ぶ。紐づいた項目の更新が会話に届き、カードの「GitHub に返信」から投稿できる。通知だけが既定で、受付にも応対させる場合はスレッドの情報欄で方針を選ぶ。引き継ぎの残作業は、引き継ぎのチェックリストか受付の `create_handoff_issues` で Issue 化すると、受け取ったスレッドから辿れる。取得間隔と自動応対の上限は [設計カード](../cards/design/issue-threads.md) を参照。
