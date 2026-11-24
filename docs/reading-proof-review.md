# 読解レビュー

既存の論文や証明を読み、その結果を非公開の読解ページにまとめる手順。手順書は `skills/reading-proof-review/SKILL.md`。ここでは使い方と注意点を書く。

ページの元になるのは、デッキのディレクトリに置く三つの JSON。

| ファイル | 内容 |
|---|---|
| `steps.json` | 証明ステップ台帳。ステップ、依存、原稿内の位置、由来、重要度 |
| `bodies.json` | ステップごとの本文 |
| `review.json` | 検証範囲、リスク、指摘とその扱い |

ページは生成物なので、直すときはこの三つを直して書き出し直す。レビュー対象の原稿は編集しない。見つけた修正点は著者が受け入れるための一覧にする。

## 台帳を検査する

MCP の `check_step_ledger` に `{"deck":"books/my-book"}` を渡す。現在のデッキなら `{}` でよい。形式、依存先、循環、目標から辿れないステップを検査する。仕様と返り値は[グラフ](graph.md)にある。

## 取得できていない資料

先行研究との比較や新規性の判断は、実際に読んだ資料にだけ基づく。使う資料は `list_ledger` と `read_ledger_attempts` で状態を確かめる（[取得台帳](ledger.md)）。

- 状態が「取得済み」の資料だけを読んだ資料として扱う。
- 「未着手」「利用者に頼む」「諦めた」の資料は、ページに「未取得」と書く。要旨や記憶、引用している別の論文から中身を補わない。
- 他人の論文の新規性は、その論文自身の主張として書く。

## 結論の書き方

主張には原稿内の位置と状態（確認済み、疑い、未確認）を付ける。「正しい」とは書かない。許す最も強い言い方は次の一文。

> no gap found within the stated scope

日本語のページでは「述べた範囲では欠落は見つからなかった」とする。範囲（どのステップと資料を見たか）を同じ文か直前の文に書く。言語モデルのレビューは専門家の確認ではなく、Lean の証明が保証するのは書かれた形式的な主張だけ。どちらもページに書く。下書き、ビルド済み、独立した読者の確認済み、公開は別の状態で、真のものだけを報告する。

## ページを書き出す

次の二つは MCP ツールで、どちらも `deck`、`slug`、`lang` が必須。

| ツール | 動作 |
|---|---|
| `export_reading_page` | `website/content/private/<slug>.md` と `website/content/private/figures/<slug>/diagram.svg` を書き、`markdown` と `diagram` を返す |
| `generate_reading_page` | 書き出しに続けて、サーバーの状態ディレクトリ下の隔離した場所で非公開 Reader を生成し、`markdown`、`diagram`、`site`、`html` を返す |

```json
{"deck":"reading/poincare-beta-proof-review","slug":"poincare-beta-proof-review-en","lang":"en"}
```

- `deck` は三つの JSON を含むワークスペース相対のディレクトリ。`slug` は小文字の英数字とハイフン（`^[a-z0-9]+(-[a-z0-9]+)*$`）、`lang` は `ja` か `en`。
- ワークスペースに実ディレクトリとして `website/content/private/` が要る（シンボリックリンクは不可）。書き出しが作るのはその下の `figures/` だけ。`generate_reading_page` はさらに `cargo` と、lab の生成器 `website/generator/`（通常ファイルの `Cargo.toml`、`Cargo.lock`、ディレクトリ `src`、`assets`、`templates`）を使う。これらをサーバーの状態ディレクトリ下 `reading-page-builds/` の隔離した場所にコピーし、そこで `cargo run --locked --manifest-path website/generator/Cargo.toml -- private` を実行する。生成器の依存クレートがその環境でビルドできる必要がある。描画は Rust で行い、`python3` は要らない。
- 非公開の出力先は `website/content/private/`。ツールが書くのはここだけで、`website/private` と `website/public` には触れない。公開用のデプロイや公開する Git 履歴に含めない。
- 各ステップには `role`（短い語句。ページの「役割」列）が必須。欠けていると書き出しが失敗する。
- 生成器の署名がない既存ファイルは上書きしない。手で書いたページは守られ、生成したページは書き出し直すと置き換わる。
- どちらのツールも公開はしない。返された `html` を開いて確かめるまで、ページができたとは言わない。

## 原稿とカードのずれ

読解カードを使う場合は `reading_sync_check` で原稿との同期を確かめる。詳細は[本のハーネス](book-harness.md)にある。

## コンパニオン CLI

`reading-companion` の使い方は `crates/fuzukue-reading/README.md` にある。失敗しやすい前提条件は次のとおり。

- `check_companion --workspace .` は、HTML が Git の作業ツリー内にあり、Git で除外されていることを要求する。Chrome/Chromium がなければ `--no-dom` を付ける。
- `companion.json` の `overview` の各項目には `id`、`name`、`role`、`input`、`output` が要る。`run_role` の `role` は `remake`、`code`、`demo` のいずれかで、役割ごとの必須項目（`panels`/`compare`/`panel_figure`、`shows`、`shows`/`truth`）も要る。
- `proof_book check` は TeX のログを読むため、先に `proof_book build` を実行する。組版には LuaLaTeX、luatex-ja、`latexmk` が要る。ビルド前は `main.log` が見つからず失敗する。
- `archive_pair` は `writing/.corpus` がリモートなしの独立した Git リポジトリでないと拒否する。
- `KATEX_DIR` がなければ数式は TeX の文字列で表示される。

証明ブックとコンパニオンの CLI コマンドを MCP から実行するには、設定済みのレシピを使う。
