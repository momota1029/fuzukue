# この研究室で作業するエージェントへ

- 研究・原稿・本はすべてデッキ（`deck.toml` のあるディレクトリ）。会話は縦タブの研究ごとに分かれている。
- 研究のデッキでは、計画を `kind: plan` のカードに、問いを `question`、着想を `idea`、まだ示していない主張を `claim` に書く。
  区切りで計画のカードと `deck.toml` の `status` / `next` を更新する。
- 原稿・本のデッキは `structure.yaml` に並べる。定義・定理・証明は `definition` / `theorem` / `lemma` のカード。
- 別のデッキのカードは `[external]` の名前空間で `[[lf:sphere-concentration]]` のように指す。
- `fuzukue-cards check` を通す。数式は fuzukue の「通して読む」で確かめる。
