# サンプルの研究室（fuzukue の新しい形）

研究・原稿・本・スライド・書類をカードのデッキとして持つ例。根に `fuzukue.toml`（研究室の目印）、各デッキの根に `deck.toml` がある。

| デッキ | kind | 内容 |
|---|---|---|
| `research/concentration` | research | 測度の集中の基礎（問い・着想・主張） |
| `research/observable-diameter` | research | オブザーバブル直径の計算 |
| `research/curvature` | research | 曲率から集中を導く |
| `papers/levy-families` | paper | Lévy 族と球面の集中 |
| `papers/obsdiam-spheres` | paper | 球面のオブザーバブル直径 |
| `books/concentration-notes` | book | 洋書を噛み砕いたノート（原著の PDF は入れない） |
| `slides/levy-talk` | slides | 論文のカードを再利用した Beamer 発表 |
| `docs/report` | document | 研究報告（テンプレートは `templates/research-report.tex`） |

絡み合い:
- 研究 concentration と observable-diameter は、原稿 levy-families を共有する。
- 研究 observable-diameter と curvature は、原稿 obsdiam-spheres を共有する。
- 研究 curvature のカードは、研究 concentration のカード（`conc:`）を参照する。
- 原稿 obsdiam-spheres のカードは、原稿 levy-families のカード（`lf:`）を使う。

起動: `cd fuzukue && cargo run -p secretaire -- examples/lab`（クライアントは README のとおり）。

## カードの形

原稿・本のカードは `cards/<種類>/<id>/` のフォルダで、`card.md`（frontmatter とノート）と `statement.tex` / `proof.tex` を持つ。
Lean は `lean/LabSample/` にあり、カード `ex-hamming-diameter` が宣言名で指している。

球面の集中（`thm-sphere-concentration`）の証明の段 2〜5 と、Lean の三角不等式（`hamming_triangle`）は、
fuzukue の「コメントを送信」の試験で送ったコメントを受けて、Claude が書いたもの（監督役が確かめた）。
