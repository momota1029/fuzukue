---
name: literate-lean
description: Write and review Lean proofs as a literate document when working on Lean packages, declarations, or formalizations of paper cards.
---

# Literate Lean

- Keep the buildable `.lean` source as the authoritative version. Use Lean module documentation (`/-! … -/`) for sections of prose and Verso-compatible Markdown with dollar-delimited math. Use the project's Verso `literate.toml` for page order and titles when it has one.
- Give public theorems, definitions, and structures a declaration docstring (`/-- … -/`). State the assumptions and conclusion faithfully. Update the docstring in the same commit when changing the statement.
- Split longer proofs at meaningful mathematical steps. Use a standalone `--` comment before each step to say what it establishes and why. Short proofs can stand on their own.
- Link a paper card to its qualified declaration in the card's `lean:` field, and put `-- card: <id>` next to the declaration. Where the project uses Lean Blueprint, use its existing `\lean{decl}`, `\leanok`, and dependency conventions in the TeX source.
- Run the package's normal Lean build. Check the declaration's axiom dependencies and read the statement against the linked card. Use `lean_doc_check` when available to check for missing and stale docstrings.
