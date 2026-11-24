---
name: reading-proof-review
description: Read an existing paper or proof as a reviewed reading page when working on a proof-review deck (steps.json, bodies.json, review.json), a step ledger, or a private Reader page. Covers the step ledger, the unacquired-source rule, the allowed verdict wording, and the export_reading_page and generate_reading_page tools.
---

# Reading proof review

- Keep `steps.json`, `bodies.json` and `review.json` in the deck directory as the authoritative version. The page is generated from them: edit the source files and export again. Never edit the manuscript under review. List corrections for the author to accept.
- Give every step an `id`, `title`, `statement`, `deps`, a `loc` in the source, a `role` (a short phrase; the page's Role column, and the page export rejects a step without it), an `origin` (`inherited`, `standard`, `adapted`, `new`) and a `tier` (`key`, `semi`, `standard`). A `key` step needs a `why_tier`: removing it breaks the theorem, and the source claims it as new. Mark a side remark the theorem does not use with `outside: true`. The target step is `T` unless `deck.toml` sets `[step_ledger] target`.
- Run `check_step_ledger {"deck": "<deck>"}` after every change to `steps.json`. Read `errors` first; `info` is `null` while the ledger is malformed, has a missing target or dependency, or has a cycle. Otherwise it gives `order`, `key_impact` and `tiers`. Treat classes as provisional until the prior-work comparison is done.

## Unacquired sources

- Before judging novelty or comparing with prior work, call `list_ledger` and, for any row you rely on, `read_ledger_attempts`. Only a source with state "acquired" (取得済み) counts as read.
- Write an unacquired, asked-of-the-user or abandoned source as **unacquired** in the page. Never fill in its contents from memory, from its abstract, or from a citing paper.
- A novelty claim is then the source paper's own claim, checked only against sources actually read. Say which ones.

## Wording of the verdict

- Every claim carries a locator in the source and a status: `confirmed`, `suspected` or `unchecked`. Never write "correct" or "proved".
- The strongest permitted statement is **no gap found within the stated scope**. State the scope (steps and sources covered) next to it, in the same sentence or the one before.
- Say who checked what. Model reviewers are not an expert's check. A Lean proof certifies only the formal statement as written; say whether someone compared that statement with the source's claim.
- Keep what the source states apart from what the page reconstructs. Write an uncounted effect as an impression, not a number.
- Drafted, built, reviewed by an independent reader, and published are different states. Report the one that is true. Neither tool below publishes.

## Running the commands

These are tools of the secretaire MCP server. Pass arguments as JSON.

| Tool | Arguments | What it does |
|---|---|---|
| `check_step_ledger` | `deck?` (defaults to the current deck) | Checks the step ledger and returns `deck`, `ok`, `errors`, `info`. |
| `reading_sync_check` | `deck?` | Compares reading cards with the recorded SHA-256 of manuscript line ranges. Returns `stale_cards`, `uncovered_theorems`, `untracked_cards`, `stale_files`. It changes nothing. |
| `export_reading_page` | `deck`, `slug`, `lang` (all required) | Renders the deck into `website/content/private/<slug>.md` and `website/content/private/figures/<slug>/diagram.svg`, and returns `markdown` and `diagram` (workspace-relative). |
| `generate_reading_page` | `deck`, `slug`, `lang` (all required) | Runs the export, then builds the private Reader in an isolated directory under the server state. Returns `markdown`, `diagram`, `site` and `html` (the page's local path). |

- `deck` for the two page tools is the workspace-relative **directory** holding the three JSON files, not a card deck. `slug` matches `^[a-z0-9]+(-[a-z0-9]+)*$`. `lang` is `ja` or `en`.
- Example, exporting then building the English page:

```json
{"deck": "reading/poincare-beta-proof-review", "slug": "poincare-beta-proof-review-en", "lang": "en"}
```

- Prerequisites. Both tools need `website/content/private/` to exist as a real directory (not a symlink); export creates only `figures/` inside it. `generate_reading_page` also needs the lab's `website/generator/` with regular files `Cargo.toml` and `Cargo.lock` and the directories `src`, `assets` and `templates`, plus `cargo`. It copies these into an isolated directory under the server state (`reading-page-builds/`), then runs `cargo run --locked --manifest-path website/generator/Cargo.toml -- private` there. The generator's crates must be buildable (vendored or cached) in that environment. Rendering is done in Rust; `python3` is not needed.
- Private output: `website/content/private/` holds unpublished pages. The tools write only there, never to `website/private` or `website/public`. Keep the directory out of public deploys and out of Git history you publish.
- Export refuses to overwrite a file that lacks the generator's signature, so hand-written pages under `private/` are safe. Re-export replaces the page it generated.
- Look at the returned `html` before telling the user the page is built. `generate_reading_page` leaves the live Reader untouched.

## Companion CLI

`reading-companion` (`cargo build --locked -p fuzukue-reading`, binary `target/debug/reading-companion`) has `indicator`, `build_review_page`, `check_review_page`, `build_prose_page`, `check_outline`, `build_companion`, `check_companion`, `check_section_record`, `build_section_page`, `proof_book {refs,build,check}` and `archive_pair`. See `crates/fuzukue-reading/README.md`. Prerequisites that fail otherwise:

- `check_companion --workspace .` needs the HTML inside a Git work tree and ignored by Git (companions are private). Add `--no-dom` without Chrome/Chromium.
- Each `overview` entry in `companion.json` needs `id`, `name`, `role`, `input` and `output`. A `run_role` needs `role` set to `remake`, `code` or `demo`, with the fields that role requires (`panels`/`compare`/`panel_figure`, `shows`, or `shows`/`truth`).
- `proof_book check` reads the TeX log, so run `proof_book build` first. Building needs LuaLaTeX with luatex-ja and `latexmk`; without a build it fails on the missing `main.log`.
- `archive_pair` refuses unless `writing/.corpus` is its own Git repository without a remote.
- Without `KATEX_DIR` pages show TeX source instead of rendered math.

Run the proof-book and companion CLI commands from MCP through configured recipes.
