---
id: card-model
kind: spec
title: カードの形式とリンク
status: accepted
links:
  - {to: workbench, rel: implements}
files: [crates/fuzukue-cards/src/lib.rs, cards.toml]
---
カード = YAML frontmatter つきの Markdown ファイル。論文、本、設計、課題、すべて同じ形式。

```yaml
id: agent-co-op            # デッキ内で一意
kind: design               # goal / decision / spec / design / plan / task / definition / theorem ...
title: ...
status: accepted           # proposed / accepted / superseded / done / stated / proved ...
links:                     # 型つきのリンク
  - {to: protocol, rel: requires}
requires: [x, y]           # 既存の論文カードとの互換（rel: requires と同じ）
defines: [集中関数, concentration function]  # このカードが定義する語と別名
files: [crates/fuzukue-proto/src/lib.rs, crates/secretaire/]   # リポジトリ内のファイル・ディレクトリ
issues: ["#123"]           # このリポジトリの Issue を指す書き方の例
```

`defines` は省略できる。[[symbols]] の定義ジャンプと参照一覧には、ここに明記した語が載る。

本文中のリンク: `[[id]]`（rel: mentions）、`[[ns:id]]`（外部デッキ）、`[[file:path#L10-20]]`。

関係の種類: `requires` / `refines` / `implements` は依存（文脈に辿る、[[context-selection]]）。
`see` / `mentions` は参照だけ。`supersedes` は古い決定を置き換えたことを示す。

デッキの設定は `deck.toml`、`cards.toml`、`harness.toml` の順に探す。
`dirs = ["spec", "notes/design", "notes/questions"]` のように複数の置き場を指定でき、各ディレクトリ以下の
YAML frontmatter 付き Markdown をカードとして読む。frontmatter のない README や既存ノートは読み飛ばす。
`files` と本文中の `[[file:…]]` はデッキの根を基準に解決する。プロジェクトの根に `deck.toml` を置く場合は
プロジェクト相対になり、カードの置き場の外にある `crates/yu-syntax/src/` なども参照できる。

既存の論文ハーネスの `harness.toml` の `[external_harnesses]` もそのまま読む。
2026-11-19 時点で `~/research/writing/*/harness` の 4 デッキ（458 枚）が
診断 0 で読める。

道具: `fuzukue-cards check | show <id> | context <id>... | files <path> | graph`。

サーバーの `GET /cards-for-file/{*path}` と `GET /source/{*path}` はプロジェクト相対パスを受け取る。
表示中のデッキから、そのパスか親ディレクトリを指すカードの `id`、`path`、`title` を返す。
`/source` はさらにファイルの `path`、`language`、UTF-8 の `text` を返し、カード一覧は `cards` に入る。

決定（2026-11-19、ユーザー）: 「もう開発段階でカードハーネスは完成させるべき」「あらゆるファイルにリンクを貼る」
