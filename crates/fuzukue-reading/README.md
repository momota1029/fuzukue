# 読解用 CLI

`reading-companion` は、旧 `reading-proof-review` の JSON・導出ブロック形式を読む Rust の CLI です。ワークスペースのルートで `cargo build --locked -p fuzukue-reading` を実行すると、バイナリは `target/debug/reading-companion` にできます。以下の例ではバイナリを PATH に置いています。`check_companion` は出力 HTML が Git の作業ツリー内で除外されていることを要求し、`proof_book check` は先に `proof_book build` が必要です。`companion.json` の `overview` には `role` が必須で、`run_role.role` は `remake`・`code`・`demo` のいずれかです。

```sh
reading-companion indicator reading/example/review.json
reading-companion build_review_page reading/example --out build/example/review.html --lang ja
reading-companion check_review_page build/example/review.html --spec reading/example
reading-companion build_prose_page reading/example/explanation.md --out build/example/explanation.html

reading-companion check_outline reading/example/companion.json
reading-companion build_companion reading/example/companion.json --out build/example/companion.html
reading-companion check_companion reading/example/companion.json build/example/companion.html --workspace .

reading-companion check_section_record reading/example/section.json
reading-companion build_section_page reading/example/section.json --out build/example/sections --original reading/example/note.pdf

reading-companion proof_book refs reading/example/proof --block P
reading-companion proof_book build reading/example/proof
reading-companion proof_book check reading/example/proof

reading-companion archive_pair reading/example/PAIR.json --workspace .
```

- **査読ページ** — `steps.json`、`bodies.json`、`review.json` から単一 HTML を生成します。証明の依存関係図、重要度に応じた折り畳み、指摘とステップを結ぶリンク、先行研究、判定を含みます。ローカルで作成した `demo.html` があれば組み込みます。本文やデモに埋め込む HTML は信頼できるものを使ってください。
- **文章ページ** — `+++` の TOML フロントマターに `title` と `slug` を書き、必要に応じて `language = "ja"` または `"en"` を指定します。Reader と同じ `::: details 要約 {#anchor}`、数式、表を使えます。単独行の `![説明](asset:figure.svg)` は、Markdown の隣にある `figures/<slug>/figure.svg` または `figures/figure.svg` を埋め込みます。
- **判定と HTML の検査** — `indicator` は六つの軸と判定の上限を検査します。同じ規則を査読ページと Reader の書き出しでも適用します。`check_review_page` は重複 ID、リンク切れ、`--spec` で指定したステップと指摘のカードを検査します。Node と KaTeX があれば数式とスクリプトの構文も検査します。`--katex-dir DIR` または `KATEX_DIR` でローカルの KaTeX を使い、スクリプト、スタイル、フォントを HTML に埋め込みます。KaTeX が見つからなければ数式は TeX の文字列で表示されます。

- **Companion** — `check_outline` は導入・全体図・難所の構成を検査します。`build_companion` は出典、AUX のラベル、コード範囲、事前収録した実行結果を単一 HTML にまとめます。`check_companion` はコードのハッシュ、外部リソース、容量、未完成の図、Git の除外設定を検査します。Chrome または Chromium を検出すると DOM の ID とリンクも検査します。ブラウザのパスは `CHROME`、DOM 検査の省略は `--no-dom`、図の制作途中の検査は `--allow-pending` で指定します。
- **節ページ** — 節の構造、翻訳、図、コードの注釈、比較、台帳を `index.html` にまとめます。節パネルとコードカードなどの TeX も出力します。コードの相対パスの基準を変える場合は `--base` を指定します。`--original` は Poppler の `pdftotext` と `pdftoppm` で原本の該当範囲を取り出します。Pandoc がある場合はインライン数式を MathML に変換します。
- **Proof book** — `book.json` の `order` に沿って導出ブロックを読み、PDF、全体とブロック別の HTML、索引、`pages.json` を作ります。LuaLaTeX と luatex-ja を含む TeX 環境、`latexmk` が必要です。オフライン数式表示に使う KaTeX の `katex.min.js`、`katex.min.css`、`fonts/` は `KATEX_DIR` で指定できます。`check` は査読記録、TeX ログ、HTML アンカーを調べ、利用可能な場合は Chrome/Chromium と Poppler でも確認します。
- **アーカイブ** — `PAIR.json` の PDF・コンパニオン・ソースと、その SHA-256 を `writing/.corpus/method-pairs/<slug>/<date>/` に保存し、`PAIR.json` に保存先を記録します。`writing/.corpus` はリモートを登録していない独立した Git リポジトリとして用意します。

未取得の原本や画像は、記録の取得状況と理由を残してください。原本の抜粋や再現結果として、推測で埋めた内容を載せることはできません。

## ノート PDF とコンパニオンを作る

既存の原稿は [LaTeX の取り込み](../../docs/book-harness.md)で読解用デッキにできます。非公開の資料を扱うときは、出力先を先に Git の除外対象にします。次の例では `reading/` を除外し、取り込み後に `reading/example/companion/companion.json` を用意します。

```sh
mkdir -p reading
fuzukue-cards import-latex manuscript/main.tex reading/example
fuzukue-cards build-latex reading/example
reading-companion build_companion reading/example/companion/companion.json --out reading/example/companion/index.html
reading-companion check_companion reading/example/companion/companion.json reading/example/companion/index.html --workspace .
```

この配置では `companion.json` の `note.pdf` に `../build/paper.pdf`、`note.aux` に `../build/paper.aux` を指定します。PDF のパスは出力 HTML からのリンク、AUX のパスは仕様ファイルからの読み込み先です。MCP の `build_paper` にデッキを指定しても PDF を組めます。HTML だけを更新するときは最後の二つのコマンドを実行します。`build_companion` は指定した AUX に参照ラベルがなければ生成を中止します。`check_companion` は PDF のパスと AUX から解決した番号・ページを HTML と照合し、古ければ再生成を求めます。

旧 `new_pair.py` の `PAIR.json`・仕様・取得記録のひな形と、旧 `build_pair.py` の成果物ハッシュの記録は、この手順では個別に管理します。`archive_pair` を使うときは `PAIR.json` に PDF と HTML のパスを記入します。

## MCP のレシピ

管理者はビルドしたバイナリをホストの固定パスに置き、[設定例](examples/recipes.toml) から使うテーブルをサーバー設定に追加できます。`list_recipes` で公開された名前を確認し、`run_recipe` にワークスペース相対パスを渡します。

```json
{"name":"build-companion","args":{"spec":"reading/example/companion.json","html":"build/example/companion.html"}}
```

レシピの実行条件は[サーバーのレシピ設定](../../docs/recipes.md)を参照してください。PDF 原本の切り出しや proof book の組版を使う場合は、レシピの隔離環境から Poppler や TeX の実行ファイル・フォーマット・フォントが見えるようにします。PDF から HTML へのリンクには絶対パスを使います。レシピで proof book を組む場合、`PROOF_BOOK_HTML_ROOT` にホスト側のプロジェクトディレクトリを指定すると、ホストで開く HTML へのリンクになります。

## 検証

```sh
cargo test --locked -p fuzukue-reading
cargo test --locked -p fuzukue-reading --test commands proof_book_builds_checks_and_links_every_formula -- --ignored
```

[テストフィクスチャ](../../tests/fixtures/reading-companion)の短いノート・節・companion は架空の例です。`proof-book-example` は `labo-skills` の `reading-proof-review` に付属する架空の例です。`expected-pages.json` は移植前の出力を参照として保存したものです。

### 移植元と検証の対応

移植元は `labo/.agents/skills/reading-proof-review/scripts/` の Python スクリプトです。表の CLI 検査は [commands.rs](tests/commands.rs)、レシピは [設定例](examples/recipes.toml) と [ブローカーのテスト](../secretaire/src/recipes/tests.rs) に対応します。

| 移植元 | Rust の実装 | CLI テストの検査項目 | レシピ |
| --- | --- | --- | --- |
| `build_companion.py` | [companion::build](src/companion.rs)、[HTML テンプレート](src/assets/companion-template.html) | AUX の表示番号・ページ、コード抜粋、実行結果を埋め込む。カスタムテンプレートにマーカーがなければ生成を拒む。 | `build-companion` |
| `check_companion.py` | [companion::check、browser_dom、anchor_problems](src/companion.rs) | ソースと埋め込みコードの差分、Git の除外設定、未完成の図と `--allow-pending`、容量上限、外部画像を検査する。再組版で古くなった PDF リンクも検出する。合成したブラウザー出力で ID の重複とリンク切れを検出する。 | `check-companion`（`--no-dom`） |
| `companion_lib.py` | [companion::load](src/companion.rs)、[feature](src/companion/feature.rs)、[figure](src/companion/figure.rs) | 出典、コードと実行結果、図とノートの参照を組み立てる。保存済みのパラメーターグリッドのキーを読み、結果が欠けた点を報告する。Node があればプリセットが指定したグリッド点を選ぶことも確認する。付属例では埋め込み JSON 全体が移植元と一致した。 | `build-companion`、`check-companion`、`check-outline` が共有 |
| `check_outline.py` | [companion::outline、outline_problems](src/companion.rs) | 導入の欠落、未定義の難所への参照、対応するステップがない難所を検出する。構成がそろえば実行単位が未記入でも通る。 | `check-outline` |
| `build_section_page.py` | [section::build](src/section.rs)、[code](src/section/code.rs)、[ledger](src/section/ledger.rs)、[original](src/section/original.rs) | 翻訳、行注釈と API カード、節パネルとコードカードの TeX を確認する。Poppler があれば原本画像の埋め込みと再利用、ページをまたぐ切り出し、最後の節の末尾までを確認する。付属例の HTML と TeX は、コードファイルを指す絶対パスをそろえると移植元とバイト単位で一致した。 | `build-section-page`、`build-section-page-with-original` |
| `check_section_record.py` | [section::check、diagnose](src/section.rs)、[code::diagnose](src/section/code.rs) | 正常な節記録と別言語版を通し、行注釈の範囲に抜けがある場合は失敗する。付属例では移植元と同じ警告 1 件で通った。 | `check-section-record` |
| `code_langs.py` | [language](src/language.rs)、[節ページでの利用](src/section/code.rs) | Julia の listings 指定と定義、Octave を明示した主版と拡張子から R を判定した別言語版を確認する。API カードの配置ではコメント内の関数名を除外する。 | 節ページの生成と節記録の検査が共有 |
| `proof_book.py` | [proof::build、check、refs](src/proof.rs)、[TeX](src/proof/tex.rs)、[HTML](src/proof/html.rs)、[査読](src/proof/review.rs) | `refs` の式本文、非表示行への参照の拒否、査読参照・TeX ログ・HTML の異常を検査する。LuaLaTeX を使うテストは式番号、`pages.json`、HTML と PDF の相互リンクを確認する。付属例のページ対応と式アンカー・PDF リンクが移植元と一致した。 | `proof-book-build`、`proof-book-check`、`proof-book-refs` |
| `archive_pair.py` | [archive::archive、snapshot](src/archive.rs) | マニフェストの SHA-256、ソースのコピー、AUX と `results/` 配下の作業ディレクトリの除外、再実行で不要になったファイルの除去、リモートのある書庫の拒否を確認する。 | `archive-pair` |

ブローカーのテストは、設定にある 10 件のレシピすべてに引数を展開します。`FUZUKUE_TEST_READING_COMPANION` にローカルのバイナリを指定すると、各レシピをフィクスチャ上で実行し、終了コード 0 を確認します。生成物の内容は CLI テストで検査します。
