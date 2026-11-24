---
id: term-lint
kind: design
title: 未定義語と表記ゆれの検査
status: accepted
links:
  - {to: card-files, rel: refines}
files: [crates/fuzukue-cards/src/term_lint.rs, crates/fuzukue-cards/src/term_lint/tex.rs]
---
## 定義語と使用箇所

カードが定義する語とその別名は、frontmatter の `defines` に並べる。

```yaml
defines: [集中関数, concentration function]
```

`defines` を省いた `kind: definition` のカードでは、主張の `\emph{...}` と
`\textbf{...}` から平文の候補を拾う。フォルダのカードでは `statement.tex`、
単一 Markdown では `## Statement`、その見出しがなければ本文の先頭から
最初のレベル 2 見出しまでを主張とみなす。数式やマクロを含む強調や、
句読点つきの文は推定から外す。`defines: []` を含め、明示した一覧を優先する。

主張・証明・ノートの本文を走査する。TeX の数式、コメント、参照や引用の引数、
Markdown のコード、既知のハイフンつきカード ID の参照は照合から外す。
連続する空白は一つにまとめる。日本語を含む語は部分文字列で照合し、
英語は大文字・小文字を区別せず単語境界で照合する。英語の語に続く日本語も境界になる。
重なる候補は左から、同じ位置なら長い語を先に採る。たとえば `Lévy 族` の中に
`族` があっても、そこでは `Lévy 族` を一語として扱う。

## 警告

- `undefined-term/used-before-definition` は、使用カードより後ろに定義カードがあるときに出る。
  順序は `structure.yaml` の `sections[].items` と、それに続く `appendix` のカード順を使う。
  並びに位置のないカードとの前後は比較しない。
- `undefined-term/missing-dependency` は、定義カードに辿りつけないときに出る。
  `Graph::context` と同じく `requires`・`refines`・`implements` を推移的に辿る。
  到達できる `ns:id` の外部定義で用語を満たせる。外部定義には現在の文書での位置を割り当てない。

同じ語を定義するカードが複数あれば、到達できる定義を優先して順序を調べる。
到達できる定義がなければ依存の警告を出し、並びに載るローカル定義のうち最初のものと比較する。
自分が定義する語は自由に使える。一つのカードで同じ語が何度出ても、警告は規則ごとに一件とする。
`Graph::diagnostics` に `Warning` として追加し、JSON の `code` で規則を識別する。
CLI の `check` は `warning[undefined-term/...]: path: message` と表示する。

## 除外と限界

既知の語として扱う場合は `deck.toml` または `harness.toml` に次のように書く。

```toml
[lint]
ignore_terms = ["集合"]
```

序論で後の定義を予告するような一枚限りの例外は、カードの frontmatter に
`lint_ignore: [Lévy 族]` と書く。どちらの除外設定も二つの規則に効く。

検査対象はデッキ内か読み込んだ外部デッキに定義がある語に限る。
TeX のマクロ展開や語義の判定は行わない。強調のない定義や別表記には `defines` を使う。
短い一般語や同じ綴りの専門語は誤検出しうる。定義側で検査対象を絞るか、
読み手が意味を知っている用例を除外で記録する。


## prh による表記の統一

デッキ直下の `prh.yml` または `prh.yaml` を表記辞書として読む。デッキに辞書がなければ
ワークスペース直下を調べる。`[lint].glossary = "dict/terms.yml"` で設定ファイルの根から見た
辞書の場所を指定できる。既存の `defines` と `ignore_terms` は引き続き使える。
`ignore_terms` とカードの `lint_ignore` には統一先の `expected` を書ける。

```yaml
version: 1
imports:
  - shared.yml
rules:
  - expected: ベンダー
    pattern: /ベンダ(?!ー)/
    specs:
      - from: ベンダを選ぶ
        to: ベンダーを選ぶ
    prh: 長音を付ける。
```

`pattern` と `patterns` は文字列または `/regex/flags` の形にできる。パターンを省くと、
統一先から英数字の全角・半角と大文字・小文字の違いを拾う。規則の `specs` は読み込み時に
変換結果を比較する。取り込み先は辞書からの相対パスで指定し、循環と失敗した `specs` は
読み込みエラーにする。正規表現は Rust で扱える構文と `gimsu` フラグの範囲を使う。
検出箇所は `term-consistency/prh` として報告し、置換候補と `prh` の補足を添える。
照合は数式やコード、TeX の参照・ラベルを除いた本文で行う。

`fuzukue-cards prh <source.yml>` で検証できる。第 2 引数に新しいファイルのパスを指定すると、
取り込み指定を含む辞書を prh YAML として書き出す。相対の取り込み先は出力ファイルから見た
位置にも必要となる。辞書の形式は [textlint-rule-prh の説明](https://github.com/textlint-rule/textlint-rule-prh#prh-format) を参照。
