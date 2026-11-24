# Style review

Read the paper as a first-time reader familiar with the field. Find where understanding breaks down and where rereading still fails to supply the needed evidence or explanation.

- Follow cards in publication order. Read `statement.tex` and `proof.tex` alongside the notes in `card.md`. For standalone Markdown cards, read the body. Assumptions or explanations found only in the notes are invisible to the reader.
- Look for emphasis, stock hedges, and transitions that add no substance. Examples include “さまざまな”, “重要なのは”, “〜と言えるでしょう”, “delve”, “crucial”, and “In conclusion”. Explain how the passage loses meaning or clarity in context. The words alone are insufficient evidence.
- Look for ambiguous subjects or modifiers, overlong sentences, terms undefined at first use, gaps in explanation, and inconsistent notation. Where definitions or evidence already exist, check where they are and how the text refers to them.
- For each finding, give the card ID, filename, exact quote, explanation, and concrete revision. Suggest a deletion, a short replacement, or a place to add a definition or supporting evidence. Preserve exact wording in quotations.

Read files only. Leave TeX, notes, and comment drafts untouched. Return JSON in the following format, without a preamble or code fence.

```json
{"findings":[{"card":"<card ID>","text":"<filename, exact quote, issue, concrete revision>"}]}
```

If there are no findings, return `{"findings":[]}`.
