# LaTeX の本をカードで編集する

## 取り込んで開く

原本の主ファイルを指定する。出力先には、親ディレクトリが存在する新しいパスを使う。

~~~sh
cargo run -p fuzukue-cards -- import-latex ~/manuscript/main.tex ~/my-book
scripts/run.sh ~/my-book
~~~

研究用のワークスペースに置く場合は、例えば出力先を ~/lab/books/my-book にして
`scripts/run.sh ~/lab books/my-book` で開く。MCP からは `import_latex_book` に
プロジェクト相対の `main` と `out_deck` を渡す。取り込みは利用者が承認してから始まる。

`structure.yaml` に部・章・節の順序、`preamble.tex` にマクロとパッケージ設定が入る。
カードには `statement.tex`、証明があるときは `proof.tex`、元ファイルと行範囲を記した
`card.md` がある。元の LaTeX ラベルは組版にも使う。GUI の「論文編集」で章順に開き、
「PDF を作る」「PDF を開く」で全体を確認する。

## AI に頼むとき

1. `graph_overview` の `depth: 1` で章を探す。章の `key` を `chapter` に渡して
   `depth: 2` でカードと一行要約を得る。続きがあれば `next_offset` を `offset` に渡す。
2. `card_read` で部位を指定する。例えば `{"card":"thm-sample","part":"proof","lines":"2-5"}`。
   行番号は部位内の 1 始まりで両端を含む。参照元・参照先は `graph_neighbors` でも探せる。
3. `card_edit` に `card`、`part`、`old`、`new` を渡す。完全一致が一箇所のときだけ
   置き換え、短い差分と、変更したカードおよび隣接カードの文体・参照の検査結果を返す。

## PDF を確かめる

取り込み時に原本とデッキを latexmk でビルドする。`my-book.import-report.md` がデッキの隣にできる。
原本とデッキの PDF 抽出本文、AUX のラベル番号・ページ・参照情報、未解決参照の警告を比較する。
`pdftotext` がない場合は、抽出本文の比較を省略して参照の比較結果を残す。
ビルドを後回しにするには CLI に `--no-verify`、MCP に `verify: false` を渡す。

~~~sh
cargo run -p fuzukue-cards -- build-latex ~/my-book
~~~

このコマンドと GUI のビルドは `~/my-book/build/paper.pdf` を生成する。
取り込みは静的な LaTeX コマンドを対象とする。マクロで作るファイル名や章見出し、
`\includeonly`、個別の latexmk 設定を使う原本は、取り込み報告と PDF を見て調整する。
画像や BibTeX の外部パス、独自クラス・パッケージの依存素材も同じ手順で確認する。

Mac では [Mac セットアップ](mac.md) に沿って、MacTeX の `latexmk` と原本のエンジンを
PATH に入れる。例と同じ upLaTeX なら `uplatex` と `dvipdfmx`、LuaLaTeX なら
`lualatex` を使う。必要なら主ファイル先頭の `% !TeX program = uplatex` などで指定する。
手元の Mac で GUI を起動し、原稿の日本語フォント、図、定理番号、章をまたぐ参照を原本と照合する。
抽出本文も比較する場合は Poppler を入れる。

### 原稿とカードのずれを確かめる

MCP の `reading_sync_check` に `{"deck":"books/my-book"}` を渡す。現在のデッキを
検査するときは `{}` で呼べる。原稿の変更や削除を `stale_cards` に、対応するカードが
見つからない定理を `uncovered_theorems` に返す。検査は原稿、カード、ハッシュを更新しない。

取り込み時に `deck.toml` へデッキ相対の `manuscript_main` を、各カードへ
`source_sha256` を記録する。ハッシュは `source`、`additional_sources`、`proof_sources`
の行範囲ごとに保存する。行番号は 1 始まりで両端を含み、行末を含む原稿のバイト列を
ハッシュ化する。各パスは原稿の主ファイルの親ディレクトリを基点に解決する。
`manuscript_main` がない場合、行範囲のパスはデッキ相対となる。古いカードの
ハッシュが記録されていない範囲は `untracked_cards` に出る。照合対象のハッシュが
一件もない場合は `ok: false` を返す。

翻訳フォルダに `source-sha256.txt` があれば、`sha256sum` 形式のファイル全体のハッシュも
照合し、差分を `stale_files` に返す。ここでのファイルパスはワークスペース相対。

### ワークスペースの原稿を一括で確かめる

MCP で `build_papers` を `{}` で呼ぶと、論文・本・スライド・書類のデッキを
ハーネスも含めて順にビルドする。失敗したデッキがあっても、最後まで実行する。
既存の PDF ビルドと同じキューで動き、完了時にデッキ、状態、エンジン、未定義参照数、
未定義引用数、overfull・underfull の合計、最初のエラーを Markdown の表で返す。
件数は各 TeX ターゲットの最終ログから数える。

Markdown の先頭には「失敗 1 / 全 3（成功 1・警告あり 1）」のような集計を置く。
失敗したデッキがあれば、その直後に先頭 5 件までのデッキ名と最初のエラーを載せる。
`structuredContent` の先頭にも `summary` があり、`total`、`success`、`warnings`、
`failed` と、先頭 5 件までの `failed_papers`（`name`、`first_error`）を返す。
この一覧ではデッキ名を 80 文字、エラーを 160 文字までに省略する。
`structuredContent.rows` と表には全デッキの診断を載せる。

バッチが完了して結果を返せた場合は、全デッキが失敗しても `isError: false` になる。
引数の不備、バッチ処理自体の失敗、指定先への結果の保存失敗は `isError: true` で返す。

`{"output":"papers/BUILD-CHECK.md"}` を渡すと、集計と表を含む同じ Markdown をワークスペース内の指定先にも保存する。
ビルドはワークスペースの一時コピーで行い、PDF や中間ファイルは完了時に削除する。
デッキ間の相対パスとワークスペース内のシンボリックリンクはコピー先でも使える。
設定で原稿や出力を指定する場合もワークスペース内の相対パスを使う。

## 読み書き量

`cargo test -p secretaire large_book_tool_bytes -- --nocapture` で生成・計測できる。
UTF-8 のバイト数を使う。カード経由はツールの引数と結果を一組ずつ数え、原稿経由は
該当章の読み込みと全文書き戻しを数える。章の用語変更では、章の全主張を読んで
該当するカードだけを編集する。

30 章、3,000 枚、原稿 6,476,244 B の計測結果。

| 作業 | 原稿 読む B | 原稿 書く B | ツール 読む B | ツール 書く B | 合計の削減倍率 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 証明 1 件 | 215,737 | 215,736 | 15,558 | 256 | 27.28 倍 |
| 参照切れ 1 件 | 215,737 | 215,734 | 15,581 | 276 | 27.21 倍 |
| 章内の用語変更 | 215,737 | 215,737 | 237,170 | 2,602 | 1.80 倍 |

章の用語変更はカードごとの呼び出しと要約ぶん、読み込みだけでは原稿の全文より増えた。
証明と参照の修正では、見取り図と編集結果を含めても原稿の読み込みだけの約 1/13.6 だった。

## labo-skills の book-harness との対応

labo-skills の `book-harness`（PR #18、マージ時点 `123eb23502ff597222cb3c31bd49e48e84e4c04c`）は、
本を共有カード（`pool/`）と並び順（`route/`）に分けて扱う。fuzukue の取り込みは別の形式を出力し、
labo-skills のツール（`import_tex.py`、`verify_import.py`、`labo book`）は呼ばない。
ここでは対応する部分と、ない部分を分けて書く。

### ファイルの対応

| fuzukue | labo-skills | 違い |
| --- | --- | --- |
| `structure.yaml` | `route/<slug>/route.yaml` | どちらも節ごとの順序を持つ。fuzukue は部・章・節の見出し TeX と、カード ID の並びを持つ。 |
| `cards/<id>/statement.tex` | `pool/<area>/<id>.md` の `## 本文` | fuzukue は元の TeX 断片をそのまま置く。labo-skills は front matter 付きの Markdown。 |
| `cards/<id>/proof.tex` | 証明カード（`proves:`） | fuzukue は証明を主張のカードに付ける。主張に付けられない証明だけ、`proof` 種別の独立カードになり、取り込み報告に警告が出る。 |
| `cards/<id>/card.md` | front matter（`id`、`kind`、`title`、`sources` など） | fuzukue は `id`、`kind`、`title`、`status`、元ファイルと行範囲を持つ。`aliases`、`requires`、`area`、`legacy`、`owner_route` はない。 |
| `preamble.tex`、`deck.toml` | なし | fuzukue はマクロとビルド設定をデッキに持つ。 |
| `<deck>.import-report.md` | `verify_import.py` の結果 | 内容は異なる（下記）。 |

fuzukue の `kind` は環境名から決める。`theorem`・`lemma`・`definition`・`proposition`・`corollary`・
`axiom`・`claim`・`remark`・`example`・`conjecture`・`exercise` と、その短縮名（`thm`、`lem` など）を
認識する。プリアンブルの `\newtheorem` と `\declaretheorem` で宣言した環境は、表示名が上の語（と日本語の
「定理」「補題」「定義」「系」「命題」）に当たれば同じ種別になり、当たらなければ `theorem` になる。
環境の外の本文は `expl`、主張に付けられない証明は `proof` になる。labo-skills の A1〜A6 の
分類とは対応しない。カードの ID は `\label` から作り、ない場合は章・節・種別・題名から作る。

### 対応していないこと

- 行範囲の指定（`--lines`）、ラベル表（`--map`）、担当ルートの指定（`--owner`）。入力は主ファイルの全体で、ラベルの有無は問わない。
- A1〜A6 への種別分類、`sol:` の証明の扱い、`aliases` と `requires`、共有カードを複数の本から使う `pool/` と `route/` の構成。
- ソース台帳（C2）、`labo book` の各コマンド、PDF の目次・しおり・HTML 見出しの検査。
- `verify_import.py` のような、項目ごとの本文比較と、取り込まれなかった行の割合の報告。fuzukue の検査は、PDF の抽出本文と AUX の比較、未解決参照の警告、`reading_sync_check` の行範囲ハッシュ照合である。
- 原本を `retired/` に移す操作。

### 移行の段階

labo-skills の移行手順を、fuzukue で行える操作に当てはめる。

1. ミラー: `import-latex` で新しいデッキを作る。原本は変えず、デッキを正本として使わない。
2. 検証: `<deck>.import-report.md` の警告と PDF の比較を読み、`reading_sync_check` を呼ぶ。
   `pdftotext` がなく比較が省略されたときは、検証済みと書かない。
3. 切り替え: 原本を編集する人がいなくなり、検査に問題がないときだけ、カードを編集する側に移る。
   再取り込みはマージではなく、新しい出力先が要る。古いデッキでの編集は引き継がれない。
4. 原本の保管: 原本のファイルを手作業で別の場所に移し、移した日と元の場所を記録する。

この手順を短くまとめた skill が `skills/book-harness/SKILL.md` にある。
