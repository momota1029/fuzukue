---
id: reading-view
kind: task
title: カードを順番に並べた画面（今の HTML の読みページの置き換え）
status: done
links:
  - {to: domains, rel: implements}
  - {to: pdf-to-card, rel: see}
files: [crates/fuzukue/src/math.rs, crates/fuzukue-view/src/lib.rs, crates/secretaire/src/cards.rs]
---
今は cards.py が HTML（website/private/cards、read.html）を作って、カードを structure.yaml の順に並べて読んでいる。
これを fuzukue の普通の画面にする。

- サーバー: デッキの `structure.yaml`（sections → items: カード id / subsection）を読んで配る。
- クライアント: 右（か中央を広げて）に、節の見出しとカード（題・本文）を順に並べる。各カードから編集・PDF の該当箇所へ。
- 数式: カードの本文は LaTeX の数式が多い。TextView は `$...$` を構文としては拾うが描かない。
  `ratex-gpui`（gpui-pdf と同じ作者、LaTeX を画像に）を TextView の数式プラグインとして組み込む。
- ハーネス独自の参照はカードへのリンクにする。たとえば `\Cref` が `card:` で始まるラベルを指す場合がある。

ユーザー（2026-11-19）:

> 「カードを順番に並べたものを今はHTMLで管理してたけどそれを普通の画面にするのがいいと思う」

実装（2026-11-19）:
- サーバーは `structure.yaml` から節・小節・カード順を読み、`Document` として送る。`harness.toml` にある `[mathjax_macros]` も同時に配信する。
- クライアントの左ペイン「論文 → 📖 通して読む」で、右に並べる（見えている範囲だけ描く `list`）。カードは主張と証明だけ（Notes / Shortcuts は作業用なので出さない）。
  見出しからカード詳細を開いて編集できる。
- 数式は `math.rs` の Markdown プラグイン（インライン・別行）で ratex-gpui に描かせ、キャッシュする。カードの詳細とチャットにも効く。
- LaTeX → Markdown の下ごしらえは `fuzukue-view`（Linux でテスト 13 件）。別行の数式とマクロ展開に対応する。対象には `\label`・`\emph`・`\textbf` も含む。カード用の `\Cref` や `\cite` にも対応する。`~` と `--` も処理する。

残り: 節や式への参照（`sec:…`、`core:eq-…`）は現在コード表示。ラベルに節題やカードを対応づけてリンクにする。
