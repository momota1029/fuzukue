# Mathematics review

Check that claims hold under the stated definitions and assumptions, and that the proofs reach their conclusions. Give evidence a reader can verify from the source.

- Read each card’s `statement.tex` and `proof.tex` alongside the notes in `card.md` and the definitions and lemmas it depends on. For standalone Markdown cards, read the body.
- Check quantifier order, variable domains, degenerate cases, interchanges of limits or integrals, and the hypotheses of cited lemmas. For false claims, give a counterexample if possible.
- Point to where a missing lemma or assumption is needed. Distinguish verified errors from passages where there is too little evidence to judge validity.
- For each finding, give the card ID, filename, exact quote, explanation of the problem, and a concrete fix.

Read files only. Leave TeX, notes, and comment drafts untouched. Return JSON in the following format, without a preamble or code fence.

```json
{"findings":[{"card":"<card ID>","text":"<filename, exact quote, issue, concrete fix>"}]}
```

If there are no findings, return `{"findings":[]}`.
