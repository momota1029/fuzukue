# この本のデッキで作業するエージェントへ

この本は、洋書の数学的な内容を、日本語のカードとして噛み砕き直したもの。**忠実な翻訳ではない**。

## カードの書き方

- 1 枚に 1 つのこと（定義 1 つ、定理 1 つ、説明 1 つ）。`kind` は definition / theorem / lemma / proposition / example / expl。
- **原著の文を写さない、訳さない。** 内容を理解したうえで、自分の言葉と自分の組み立てで書く。記号は本全体でそろえる。
- 出典は `source: books/<原著>.pdf#<ページ> <原著での番号>` に必ず書く（例: `books/original.pdf#15 Theorem 1.4`）。
  原著の PDF は `books/` に置くが、リポジトリには入れない（.gitignore）。
- 使う定義・補題は `requires` に書く。本文の参照は `\Cref{card:<id>}`。
- 証明は段に分け、各段で何を使ったかを書く。原著の証明に飛躍があれば埋める。埋められなければ `## Notes` に書いて status を `gap` にする。
- status: `draft`（起こしただけ）→ `revised`（推敲済み）→ `checked`（人が確かめた）。`checked` は人だけが付ける。

## 章の並び

`structure.yaml` に章（sections）と、カード・小節（items）を並べる。新しいカードを作ったら、ここにも入れる。

## 作業の単位

章や節ごとに GitHub の Issue にする（fuzukue の docs/workshop.md の手順）。Issue には、原著のページ範囲と、作るカードの見込みを書く。
原著は `pdftotext -f <開始> -l <終了> books/<原著>.pdf -` で、その範囲だけを読む。

## 確かめ方

- `fuzukue-cards check`（リンク切れ、id の重複）を通す。
- 数式は fuzukue の「通して読む」画面で表示を確かめる。
