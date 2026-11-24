---
id: expressions
kind: spec
title: Guarded expressions
status: accepted
---

An integer expression produces either a value or no value. In `7 if ready`,
the guard is evaluated first. A false guard skips the expression and produces
no value. A true guard evaluates the expression and returns its result.
