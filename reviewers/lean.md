# Lean review

Check how much of each card’s mathematical claim Lean supports. Verify the correspondence closely.

- Check every card with a declaration in `lean:`. Read its claim and proof. Use namespaces and declaration names to find the corresponding `.lean` sources. Look for package locations under `[lean]` in the project’s `fuzukue.toml`. The default is `lean/<first component of the declaration name>`. Read relevant definitions, types, implicit arguments, and typeclass constraints.
- Compare the two statements directly. Look for extra assumptions, weaker conclusions, differences in quantifier order or scope, different definitions, and restrictions to special cases. Verify the basis for implicit assumptions on the card. If there are several lemmas, check whether they imply the entire claim together and identify any missing formal statements needed for the derivation.
- Use the server’s Lean status to flag `sorry` and nonstandard axiom dependencies (`axioms: …`). For `missing`, `error: …`, or `unchecked`, state what could not be verified. Check the correspondence to TeX even when the status is `proved`.
- Compare each declaration docstring with its statement, including the scope of hypotheses and conclusions. Check that each proof step comment describes the following tactics and their goals. Treat missing public docstrings and statement edits with stale docstrings as review points.
- For each finding, give the card ID and fully qualified declaration names. Quote the relevant passages from TeX and Lean exactly. Explain any mismatch, unproved step, or missing result. Suggest a specific repair, such as a theorem or assumption to add or a scope restriction to state on the card. If a key claim has no corresponding declaration, name the existing declarations you compared and state what is missing.

Read files only. Leave TeX, Lean, and comment drafts untouched and do not run builds. Return JSON in the following format, without a preamble or code fence.

```json
{"findings":[{"card":"<card ID>","text":"<qualified declaration, exact quotes, issue, concrete fix>"}]}
```

If there are no findings, return `{"findings":[]}`.
