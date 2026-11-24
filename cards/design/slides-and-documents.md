---
id: slides-and-documents
kind: design
title: カードからスライドと書類を組み立てる
status: accepted
links:
  - {to: card-files, rel: refines}
  - {to: books, rel: refines}
files: [crates/secretaire/src/build.rs, crates/secretaire/src/synctex.rs, examples/lab/slides/levy-talk/structure.yaml, examples/lab/docs/report/structure.yaml, examples/lab/templates/research-report.tex]
---
スライドと文書を成果物として扱う。

[[card-files]] と [[books]] と同じように、デッキの `structure.yaml` に並べたカードを
TeX の `\input` で読み込む。MCP の `build_paper` にデッキのパスを渡すと、
uplatex + dvipdfmx で PDF を組み立ててそのパスを返す。SyncTeX は、PDF 上の位置を
元のカードの TeX に対応づける。どのデッキでも `[macros]` の定義が使える。

`build_paper` の応答には `ok`、`pdf`、`log` に加えて `pdf_audit` が入る。
ビルド成功時は各出力 PDF のパスと、内部リンク（`links`）、しおり（`bookmarks`）、
SyncTeX（`synctex`）の点検結果を返す。複数出力がある場合も `pdf` は最初の出力を指す。
各点検には `status`（`pass`、`fail`、`incomplete`）と `findings`（`code`、`location`、
`detail`）が入る。内部リンクとしおりは PDF のオブジェクトを読み取って点検する。
SyncTeX は補助ファイルのページ範囲と、抜き取った位置の順引き・逆引きを確認する。
SyncTeX の実行ファイルがなければ、SyncTeX の点検は
`incomplete` になり、`missing_tool` の指摘に `tool: "synctex"` が入る。
点検結果は警告として別欄に表示され、ビルドの成否、MCP の `isError`、ログ、PDF の受け渡しには影響しない。
`ok: false` を返すビルド結果では、`pdf_audit` は空配列になる。

## スライド

- `kind = "slides"` で Beamer の `build/slides.pdf` を作る。先頭に表紙が入る。
  `[slides]` に `theme`、`aspect_ratio`（例 `"16:9"`）、`title`、`author`、`date` を指定できる。
- `sections` の各節で `frames` にカード ID を並べる。`slide.tex` があればそのファイルを使い、
  なければ `statement.tex` を使う。定理型カードの主張は定理環境に入る。
- `{frame: "題", cards: [id1, id2]}` で同じフレームに複数のカードを置く。
  `{frame: "題", tex: "file.tex"}` ではデッキ相対の TeX 断片を読み込む。
- `[external]` で取り込んだカードは `lf:def-mm-space` のように名前空間つきで指定する。

## 書類

- `kind = "document"` で `build/document.pdf` を作る。定理型カードは定理環境に入れ、
  `expl` や `note` の `statement.tex` は地の文として置く。
- `template` は `article`（既定の jsarticle）、`report`（jsreport、各節を章にする）、`letter`、
  またはプロジェクト相対の TeX テンプレートを指定する。
- 独自テンプレートは文書クラス、必要なパッケージ、使用する定理環境を定義し、本文の位置に
  `%%LABO-BODY%%` を置く。マクロはプリアンブルの `%%LABO-PREAMBLE%%` に入る。
  このマーカーを省くと `\begin{document}` の直前に入る。
