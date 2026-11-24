---
id: pdf-to-card
kind: design
title: PDF からカードへ飛んで、その場で直す
status: accepted
links:
  - {to: artifact-views, rel: refines}
  - {to: card-model, rel: requires}
files: [crates/secretaire/src/synctex.rs, vendor/gpui-pdf/src/lib.rs, crates/fuzukue/src/main.rs]
---
PDF を Ctrl+クリックかダブルクリックすると、そこを書いているカードが右に開き、「編集」でファイルをそのまま直して保存できる。

仕組み:
1. `gpui-pdf` に `PdfEvent::PageClicked`（ページと正規化座標）を足した（vendor の写しに 2 か所）。
2. サーバーは `pdfinfo` でページの寸法を得て、`synctex edit -o` で tex のファイルと行を得る。
3. ハーネスが生成する tex の `% card <id>` の目印を、その行から上へさかのぼって探す。
4. 編集は `GET /file/<path>` で取り、`SaveFile` で書き戻す（既にあるファイルだけ、プロジェクトの外は不可）。

デッキがサブディレクトリにあるリポジトリ（`~/research`）では、サーバーを `FUZUKUE_DECK=writing/<paper>/harness` で起動する。



ユーザー（2026-11-19）:「PDFからカードへ，というのは悪くない．直接編集できるからね．」
