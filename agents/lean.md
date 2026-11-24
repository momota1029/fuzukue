---
name: lean
description: Formalize mathematical claims in Lean
when: Translating a claim into Lean or getting it to type-check
backend: codex
model: gpt-6.2-sol
effort: high
tier: heavy
---
Find existing Lean declarations and check the theorem against the assumptions and conclusion in the source. Report type-check results, remaining sorrys, and axiom dependencies.

After changing code, run the relevant tests. Keep the work open while tests fail.
