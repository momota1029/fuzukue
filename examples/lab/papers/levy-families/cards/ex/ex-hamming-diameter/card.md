---
id: ex-hamming-diameter
kind: example
title: Hamming 立方体の直径
status: stated
requires: [def-levy-family]
lean:
- LabSample.Hamming.hamming_le_length
- LabSample.Hamming.hamming_map_not
- LabSample.Hamming.hamming_triangle
---
## Notes
Lean: 上からの評価（hamming_le_length）、反転で直径を実現すること（hamming_map_not）、三角不等式（hamming_triangle）すべて証明済み（sorry なし）。
三角不等式は、長さが違うと打ち切りで偽（`[a]`, `[]`, `[b]`）なので、同じ長さの仮定 `xs.length = ys.length`、`ys.length = zs.length` を足した。
