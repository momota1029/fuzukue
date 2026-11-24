---
id: references
kind: design
title: 参考資料をフォルダに置き、抽出テキストを検索する
status: accepted
links:
  - {to: card-files, rel: see}
files:
  - crates/secretaire/src/references.rs
  - crates/secretaire/src/references/bibliography.rs
  - crates/secretaire/src/references/search.rs
  - crates/secretaire/src/references/fetch.rs
  - crates/secretaire/src/references/fetch/sources.rs
  - crates/secretaire/src/mcp.rs
---

> referencesというフォルダに色々資料を入れておいて，corpusみたいなものも作っておくと良いと思います．これは単純な仕組みとして作っておくと良いです．

## 資料の置き方

プロジェクトの `references/<key>/` に、資料ファイルと書誌情報をまとめる。従来の `reference/<key>/` も読める。両方のルートがあるときは `references/` を使う。`_` や `.` で始まるディレクトリは飛ばす。

新しい資料では `ref.toml` に `title`、`authors`、`year` を書ける。`bib_key` と `source` も任意で書ける。
`ref.toml` がなければ `reference.md` の YAML frontmatter を読む。従来の本文だけのノートでは、見出しや `Title:`、`Authors:` などの欄から書誌情報を拾う。ファイル一覧は実際の資料フォルダを走査して作る。

## コーパスと道具

サーバーは起動時と資料の変更時に `<refs-dir>/.corpus/<key>/` をバックグラウンドで更新する。PDF は `pdftotext -layout` で抽出してページごとに、UTF-8 の TXT・Markdown・TeX は 60 行ごとに保存する。入力ファイルのサイズと更新時刻を `.corpus/manifest.json` に記録し、変更のあったファイルだけを抽出する。生成物は `.corpus/.gitignore` で除外する。

- `list_references {}` は書誌情報、ファイル、抽出状態を返す。状態は `ready`（抽出済み）、`partial`（一部抽出済み）、`pending`（未抽出）、`empty`（抽出対象なし）。
- `search_references {query, limit?}` は日本語を部分文字列、英語を大文字小文字を区別しない単語として照合し、TF-IDF で並べる。キー、ファイル、ページまたは行範囲、スコア、抜粋を返す。既定は 8 件、最大は 50 件。
- `read_reference {key, file?, pages?}` は抽出済みの本文を返す。`file` を省くと PDF を優先する。`pages` は `3` や `3-5` の形で、PDF ではページ、テキストでは行を指定する。両端を含み、番号は 1 から数える。本文が 24,000 バイトまたは 64 部分を超えると `truncated` が立つ。
- `refresh_references {}` はその場で差分更新し、抽出、スキップ、削除の件数とエラーを返す。

資料ディレクトリがあるプロジェクトの Claude に、これらの道具をシステムプロンプトで案内する。

## 書誌の点検

読み取り専用の MCP ツール `check_bibliography {}` はプロジェクト直下の `index.bib` を点検する。原稿の書誌を調べるときは `check_bibliography {"bib":"papers/example/references.bib"}` のように UTF-8 の `.bib` ファイルへのプロジェクト相対パスを渡す。

- `matches` は、BibTeX キーとディレクトリ名が大文字・小文字も含めて一致した行。`bib_only` と `directory_only` は片方にしかないキーを返す。`candidates` は大文字・小文字と記号の違いを除いてキー名を比較した候補で、近い順に最大 3 件を返す。
- `references` は各ディレクトリの `reference.md`、PDF、テキストの有無を返す。`reference.md` は直下、PDF とテキスト（`.txt` または `.tex`）はサブディレクトリも調べる。
- `missing_required` は BibTeX 標準で必須の項目が空か省略されているエントリを返す。`@inbook` の `chapter` と `pages` のような選択項目は、いずれか一方があればよい。`@article` の `pages` は任意。標準外のエントリ種別は `unknown_types` に出る。 [BibTeXing §3.1](https://mirrors.cstcloud.cn/CTAN/biblio/bibtex/base/btxdoc.pdf)

## 外部データベースからの取り込み

> 本当はMathSciNetとかから論文を取ってきたりもしたい

- `find_papers {query, source?, limit?}` は zbMATH Open、arXiv、Crossref を検索する。`source` は `zbmath`、`arxiv`、`crossref`、`all`（既定）。候補には題、著者、年、掲載先、判明した識別子、取得用の `id`、arXiv PDF の有無が含まれる。既定は 8 件、最大は 50 件。複数の検索先のうち一部が失敗した場合は、取得できた候補と検索先ごとのエラーを返す。
- `fetch_reference {id, key?}` は `arXiv:2301.01234`、`doi:10.…`、`zbmath:1234.56789`、`mr:MR1234567` などの ID を受け取る。`references/<key>/ref.toml` に取得できた書誌情報、識別子、URL、BibTeX を保存する。保存先を省くと、同じ識別子を持つ資料を再利用し、新規資料には `ledoux2001` のようなキーを付ける。使用済みのキーには連番を付ける。明示したキーが既存フォルダを指す場合は、同じ識別子を持つ `ref.toml` があるときだけ更新する。
- PDF の自動取得先は `arxiv.org`。直接指定した arXiv 論文に PDF がある場合や、他の書誌データに arXiv 版への相互参照がある場合に取得する。PDF を取得できなかった場合も書誌情報を保存し、理由を返す。保存後は既存のコーパスを差分更新する。

MathSciNet の書誌サービスには機関購読が必要で、利用規約は自動検索と自動ダウンロードを制限している。MR 番号の照合先は zbMATH Open に限る。一致する相互参照が得られなければ、MR 番号と利用者がブラウザーで開くための MathSciNet URL を保存する。 [MathSciNet 利用規約](https://mathscinet.ams.org/mathscinet/help/mathscinet_terms_of_use.html)

取り込み先は常に `references/`。従来の `reference/` だけを使っていた場合も、取り込み後のコーパスは既存の優先順位どおり `references/` を参照する。

HTTP の User-Agent は個人情報を含まない `fuzukue (references fetcher)`。各検索先への通信にタイムアウトと間隔を設け、arXiv へのリクエストは 3 秒以上空ける。zbMATH Open 由来の書誌情報の利用条件は CC BY-SA 4.0。 [arXiv API 利用条件](https://info.arxiv.org/help/api/tou.html) [zbMATH Open API 利用条件](https://static.zbmath.org/legal/api-terms-and-conditions.html)
