---
id: books
kind: design
title: 洋書の数学的内容を噛み砕いた日本語の本をカードで作る
status: accepted
links:
  - {to: domains, rel: refines}
  - {to: reading-view, rel: requires}
files: [templates/book/, scripts/new-book.sh, scripts/run.sh, docs/mac.md]
---
忠実な翻訳ではなく、原著の数学的な内容を理解したうえで、日本語のカード（定義・定理・証明・説明）として書き直す。
仕組みは論文のハーネスと同じ（structure.yaml の章立て、カード、通して読む画面、数式）。

- カードの `source:` に原著の位置を書く（`books/original.pdf#15 Theorem 1.4`、または {pdf, page, item} の表）。
  カードの「原著 p.N」で、右に原著のそのページが開く（gpui-pdf の reveal_highlight、ページが光る）。
- 原著の PDF は `books/` に置き、リポジトリには入れない（.gitignore）。原文を写さない・訳さない方針は、ひな形の AGENTS.md に書いた。
- 章や節ごとに Issue にして、作業者が `pdftotext -f -l` でその範囲だけ読み、カードを起こす（[[issue-queue]]）。
- 本を作るのは Mac を使う友人。Mac ではサーバーとクライアントを同じ Mac で動かす（`scripts/run.sh`、[[file:docs/mac.md]]）。

書籍の解説や翻訳をカードで管理し、macOS から編集できるようにする。

確かめた（2026-11-19）: ひな形から作ったデッキで、カードの「原著 p.15」→ 原著の 15 ページが開く（Windows のクライアント）。
サーバーの macOS 向けコンパイル（cargo check --target aarch64-apple-darwin）。
確かめていない: Mac でのクライアントのビルドと動作、scripts/run.sh を Mac で。
