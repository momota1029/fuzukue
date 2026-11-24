# 研究リンクの実験

fuzukue のリポジトリ直下から実行する。Rust と Cargo が必要。

```sh
cargo run --release --locked --manifest-path tools/research-links/Cargo.toml -- /path/to/research
```

入力は読み取り専用。走査するのは UTF-8 の Markdown、TeX、Lean、BibTeX、プレーンテキスト。隠しディレクトリ、`build`、`target`、`output`、`node_modules`、シンボリックリンクは対象外。索引はメモリに保持する。実行結果は標準出力に相対パスと行番号で出す。表示する JSON 相当サイズは、文書・定義・参照・問いを数え上げた値。

モードを指定するには、入力パスの後ろにオプションを足す。

| オプション | 出力 |
| --- | --- |
| `--find def:product` | 名前が一致する定義と参照。Lean は完全修飾名または末尾名を指定できる。 |
| `--between reading/ research/ --limit 10` | パスの部分一致を使って候補を絞る。同じ二つのプロジェクトから最大2件を表示する。 |
| `--exclude /reviews/` | 指定した文字列を含む候補パスを除く。複数回指定できる。 |
| `--include-linked --include-duplicates` | 既存の参照でつながるペアと、同じ内容のファイルのペアも候補に含める。 |
| `--questions '' --limit 10` | 問いの見出しと関連する別フォルダの文書。空文字の代わりに見出しやパスの一部でも絞れる。 |
| `--audit tex_macro,naming_phrase,use:tex_ref --limit 5` | 種別・抽出元ごとに最大5件の等間隔サンプル。参照は `use:` を付ける。 |

カードは `fuzukue-cards` で読む。TeX と Markdown の定義語、Lean 名の単語列を使って候補を出す。多くの文書に出る語は除外し、珍しい語と定義元との一致を重くする。Lean 名の照合は字句単位で、参照先を絞れない場合は未解決として残す。TeX の展開や Lean のコンパイルは行わない。

実データでの結果は [実験カード](../../cards/design/research-links-trial.md) にある。
