---
id: postfix-if
kind: decision
title: Evaluate postfix guards before the value
status: accepted
requires: [expressions]
files: [src/postfix_if.rs, tests/postfix_if.rs]
---

Pass the guarded expression as a closure. The evaluator calls it only when
the guard succeeds, so a skipped expression cannot produce an error.
