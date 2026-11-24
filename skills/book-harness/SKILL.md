---
name: book-harness
description: Move an existing LaTeX book into fuzukue cards and keep it in step with the original when working with import_latex_book, an imported book deck (structure.yaml, statement.tex, proof.tex, card.md), or reading_sync_check. Covers the mirror, verify and cut-over rules and how they relate to labo-skills' book-harness.
---

# Book harness

Guide: `docs/book-harness.md` in the fuzukue repository. This skill follows the migration rules of the upstream `book-harness` skill (labo-skills, PR #18, merge revision `123eb23502ff597222cb3c31bd49e48e84e4c04c`, `skills/book-harness/references/migration.md`). fuzukue does not run that skill's tools. It has its own importer and its own card layout, so read the mapping section of the guide before carrying upstream commands over.

## What fuzukue does

- `import_latex_book {main, out_deck, verify?}` (CLI: `fuzukue-cards import-latex <main.tex> <out-deck> [--no-verify]`) expands the original's `\input`/`\include` files and writes a new deck. `out_deck` must not exist yet; the user approves the import before it starts.
- The deck holds `structure.yaml` (parts, chapters, sections, card order), `preamble.tex`, and one directory per card with `statement.tex`, `proof.tex` when there is a proof, and `card.md` (id, kind, title, source line ranges). Sibling `<deck>.import-report.md` records warnings and the PDF comparison.
- The importer takes everything it can parse, labelled or not. It does not take a label catalogue, a line range or an owner.
- `build-latex <deck>` (or the GUI) assembles `build/paper.pdf` from the cards.

## Rules

1. **Mirror first.** Treat the first import as a read-only mirror. The original manuscript stays the authoritative text. Do not edit cards while people still edit the original, or the two drift apart.
2. **Verify before relying on it.** Read `<deck>.import-report.md`: warnings, unresolved references, and the PDF text and AUX comparison. Run `reading_sync_check` on the deck; read `stale_cards`, `uncovered_theorems`, `untracked_cards` and `stale_files`. It changes nothing. Without `pdftotext` the text comparison is skipped; say so rather than reporting a match.
3. **Cut over only with the writers stopped.** Switch to editing cards when nobody edits the original and the checks are clean. Re-import is not a merge: it needs a new `out_deck`, and card edits made in the old deck are not carried over. Keep the original files (the upstream skill moves them to `retired/`; fuzukue has no command for it, so do it by hand and record it).
4. **Keep old names.** Card ids come from the original LaTeX labels where there are any. Do not rename them without checking the references.
5. **Report what is true.** Imported, verified, and cut over are different states. A report that is missing or a comparison that was skipped means "not verified".

## Differences from upstream

fuzukue has no `--lines`, `--map` or `--owner`, no `pool/` and `route/` layout, no A1-A6 classification, no `labo book` commands and no `verify_import` coverage report. Do not describe them as available. The guide lists each difference and the migration stages in fuzukue's terms.
