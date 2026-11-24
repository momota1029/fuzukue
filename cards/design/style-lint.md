---
id: style-lint
kind: design
title: カードの文体を検査する
status: accepted
links:
  - {to: term-lint, rel: see}
  - {to: reviewers, rel: see}
files:
  - crates/fuzukue-cards/src/style_lint.rs
  - crates/fuzukue-cards/style/default.toml
---

> 「正直人間が文章を書くよりもポン出しした論文のクオリティが高いほうが望ましいと思います．」

カードを書いた直後に機械的に拾える文体上の問題を検査し、その後に文体査読を行う。
警告は該当箇所を引用し、見直す方向を示す。文章の修正は書き手が行う。

## 規則

対応先とプリセットの既定値は、[日本語技術文書プリセット](https://github.com/textlint-ja/textlint-rule-preset-ja-technical-writing#ルール一覧) と
[AI ライティングプリセット](https://github.com/textlint-ja/textlint-rule-preset-ai-writing#含まれるルール) に基づく。
既存の診断コードを維持し、対応する規則には `textlint_rule` を付ける。表示例は
`style/long-sentence (textlint: sentence-length)`。文体査読の下書きには修正の目安を添える。カード表示は件数を折りたたみ、展開すると抜粋と指摘箇所を示す。

| ネイティブ診断 | textlint 規則とプリセットの既定値 | fuzukue の判定と差分 |
| --- | --- | --- |
| `style/long-sentence` | `sentence-length`、100 字 | 日本語は空白と除外箇所を外して 100 字、英語は従来どおり 40 語を上限にする。 |
| `style/comma-density` | `max-ten`、3 個 | 日本語の一文で `、` と `，` を合わせて 3 個まで。和文 TeX の全角カンマにも対応する。 |
| `style/max-comma` | `max-comma`、3 個 | 一文の半角カンマを 3 個までにする。 |
| `style/max-kanji-continuous-len` | `max-kanji-continuous-len`、6 字 | 漢字・踊り字などの連続を 6 字までにする。固有名詞などはカード単位で除外できる。 |
| `style/no-mix-dearu-desumasu` | `no-mix-dearu-desumasu`、見出しは自動、本文は敬体、箇条書きは常体を優先 | 文末の代表的な活用を照合し、既定ではカード内の多数派にそろえる。同数なら先に使った文体を優先する。研究原稿と説明文のどちらにも使えるよう、本文・箇条書きに同じ設定を適用する。 |
| `style/ja-no-mixed-period` | `ja-no-mixed-period`、`。` | 日本語の文末の `。`、`．`、`.` を設定値にそろえる。箇条書きの断片や数式前の文も扱うため、句点の欠落はネイティブでは検査しない。 |
| `style/no-doubled-joshi` | `no-doubled-joshi`、`min_interval: 1` | 漢字・カタカナ・英数字で終わる名詞相当の並びの直後を見て、「私は彼は」や「学校のの」を拾う。かなだけの語や複数の助詞が続く場合の品詞判定は外部の規則に任せる。 |
| `style/no-double-negative-ja` | `no-double-negative-ja`、有効 | 「なくもない」「ないことはない」などの明示的な構文を拾う。単純な対比は別の規則で扱う。 |
| `style/ja-no-weak-phrase` | `ja-no-weak-phrase`、有効 | 「かもしれない」「と思います」など代表的な定型を照合する。必要な留保は残せるよう、警告にする。 |
| `style/ja-no-redundant-expression` | `ja-no-redundant-expression`、有効 | 「することができる」などを照合する。サ変名詞一般の判定は外部の規則に任せる。 |
| `style/no-exclamation-question-mark` | `no-exclamation-question-mark`、有効 | 半角と全角の感嘆符・疑問符を拾う。 |
| `style/no-hankaku-kana` | `no-hankaku-kana`、有効 | 半角カタカナと半角の濁点・半濁点を拾う。 |
| `style/ai-phrase` | `no-ai-hype-expressions`、有効。対応は一部 | 誇張表現と日英の定型句の辞書を照合する。英語は語境界で大文字・小文字を区別せず、`furthermore` はカード内の 2 回目に警告する。 |
| `style/repeated-ending` | 直接の対応なし | 同一段落で同じ文末が 3 文続いたときに警告する。`ja-no-successive-word` は語の重複、`no-doubled-conjunction` は接続詞の重複を扱う。 |
| `style/rhetorical-negation` | 直接の対応なし | 「ではなく」や `not X but Y` 型の対比を拾う。数学的に必要な対比を残せるよう見直しを促す。 |
| `style/hedging` | `ja-no-weak-phrase` と対象が一部重なる | 日英で留保が重なる定型を拾う。留保 1 回を拾う規則とは別に有効化できる。 |
| 外部のみ | `arabic-kanji-numbers`、`no-dropping-the-ra`、`no-doubled-conjunctive-particle-ga`、`no-doubled-conjunction`、`ja-no-successive-word`、`ja-no-abusage`、`ja-unnatural-alphabet`、`no-unmatched-pair`。既定で有効 | 正確な判定に品詞、辞書、Markdown の構造などを使う。 |
| 外部のみ | `no-nfd`、`no-invalid-control-character`、`no-zero-width-spaces`。既定で有効 | 外部に任せ、ネイティブの文体検査は文の構造と表現を扱う。 |
| 外部のみ | `no-ai-list-formatting`、`no-ai-emphasis-patterns`、`no-ai-colon-continuation`、`ai-tech-writing-guideline`。既定で有効 | 機械的な箇条書き、太字による強調、コロンでの接続や文書構成を検査する。外部検査には見出しと Markdown の箇条書き・強調を渡す。 |

## 走査対象

ローカルカードの主張・証明・ノートを読む。未定義語 lint と同じ TeX 走査器を使い、
数式、TeX コメント、参照・引用マクロの引数、コードを除く。診断の引用中では除外した箇所を
`…` と表示する。見出しや箇条書きの項目、空行で段落を区切る。
段落内の一行改行は行の折り返しとし、和字に挟まれていれば空白を挟まずにつなぐ。
文は `。．！？.!?` で区切る。英語の小数点や既知の略語のピリオドは文の途中として扱う。

Markdown の引用行を除く。Notes では「ユーザー」の明示的な引用、行全体がかぎ括弧の引用、
出典・Lean 宣言・著者の指示など、由来を示す定型の行も除く。原稿の文体と記録の文体は
用途が異なるためである。印のない引用や複数行に続く引用の範囲は推定しない。
TeX マクロの展開や文の意味の判定も行わない。

## 設定と実行

`deck.toml`、`cards.toml`、`harness.toml` のうち、デッキ設定として選ばれたファイルに
`[style]` を置く。

```toml
[style]
extra_phrases = ["特筆すべきは"]
disable = ["rhetorical-negation"]
max_ja_chars = 100
max_en_words = 40
max_ja_commas = 3
max_ascii_commas = 3
max_kanji_chars = 6
period_mark = "。"
prefer_register = "auto" # auto | plain | polite
```

デッキ直下の `style.toml` でも同じキーを設定できる。トップレベルか `[style]` に書く。
句と無効化の一覧はデッキ設定に追加し、しきい値は `style.toml` の値を優先する。
`[[rule]]` だけの既存の手書き台帳は lint 設定として扱わない。
`disable` とカードの `lint_ignore` には診断コードと textlint の別名のどちらも使える。
たとえば `lint_ignore: [style/rhetorical-negation, max-ten]` で一枚限りの例外を指定する。

ワークスペースの `fuzukue.toml` で外部の textlint を設定できる。

```toml
[style]
textlint = "auto" # auto | off | 実行ファイルと引数
```

`auto` は既定値で、ワークスペースに `.textlintrc`、`.textlintrc.json`、`.textlintrc.yml`
のいずれかがあるときに追加検査を行う。ローカルにある textlint、PATH 上の textlint、
追加インストールを禁止した npx の順に利用できる実行方法を探す。
カスタムコマンドはシェルを経由せずワークスペースを作業ディレクトリにして起動する。
`off` は外部検査を無効にする。起動や設定に失敗した場合は状態を表示し、ネイティブの結果は使える。

共通の TeX 走査器で隠したカード本文を一時 Markdown に書き出し、textlint の JSON 結果を
組み立て済みカード本文の行と UTF-16 列に戻す。規則が同じで外部結果の位置がネイティブの該当範囲内に
入る場合は、ネイティブの結果を残す。textlint の設定ファイルと規則はワークスペースで管理する。

`fuzukue-cards --root <deck> check --only style` で文体警告だけを表示する。
`--only` は表示、件数、診断による終了コードを指定したコード接頭辞で絞る。
警告は `Graph::diagnostics` に入り、MCP の `check_cards` でもコード、規則名、行と列、修正の目安を返す。
