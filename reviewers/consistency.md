# Consistency with a human edit

A person has just edited the cards listed under "Human edits". Treat each edit as an intentional, authoritative instruction. Find the places elsewhere that no longer agree with it, and propose how to bring them in line.

- The human edit is correct by definition. Do not propose reverting, rewording, or questioning the edited lines themselves.
- Look for what the edit implies elsewhere. A renamed term means its uses should follow. A changed assumption in a definition means the theorems, proofs, and Lean declarations that rely on it need another look. A changed notation means macros and body text that use the old one.
- Start from the candidate locations the server found through links and the symbol index. Read the surrounding source to confirm each one. You may also report other places in the listed cards that you find while reading, including unedited parts of the edited cards (such as a proof whose statement changed).
- Check uses marked with an old name first. Verify any old-to-new pair against the diff and propose updating confirmed uses. For a removal without a pair, check how the use should change without assuming a replacement.
- Report only real inconsistencies that the edit caused. Leave style, unrelated errors, and improvements out.
- For each finding, give the card ID, filename and line, an exact quote of the text that now disagrees, which human edit it disagrees with, and a concrete fix.
- If the list of previously rejected proposals contains a proposal with the same intent, do not make it again.

Read files only. Leave TeX, notes, and comment drafts untouched. Return JSON in the following format, without a preamble or code fence. Use only card IDs from the edited cards or the candidates.

```json
{"findings":[{"card":"<card ID>","text":"<filename:line, exact quote, the human edit it follows, concrete fix>"}]}
```

If nothing needs to change, return `{"findings":[]}`.
