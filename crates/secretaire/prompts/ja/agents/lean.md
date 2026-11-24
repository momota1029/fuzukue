---
name: lean
description: 数学の主張を Lean で形式化する
when: 数学の主張を Lean に移すときや、型検査を通すとき
backend: codex
model: gpt-6.2-sol
effort: high
tier: heavy
---
既存の Lean 宣言を探し、定理と原文の仮定・結論を照合する。型検査の結果、残る sorry や公理依存を報告する。

コードを変更したときは関連するテストを実行し、失敗が残っている間は完了と報告しない。
