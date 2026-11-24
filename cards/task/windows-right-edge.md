---
id: windows-right-edge
kind: task
title: Windows（表示倍率 250%）でウィンドウ右端の約 29px が描かれない
status: open
links:
  - {to: server-client-split, rel: see}
---
2026-11-19 の確認: 画面 3840x2160、DPI 240（250%）。`GetClientRect` は 3500x2108（= 1400 論理px でレイアウトどおり）なのに、
client 領域は右端から約 72 物理 px（約 29 論理 px）が黒くなる。その範囲にかかる要素は切れる。
レイアウト側（左 260 + 中央 + 右 45%）は client 幅に収まっているので、GPUI の Windows の描画（非整数倍率でのスワップチェーンの大きさ）を疑う。

ユーザーに実画面を見てもらい、スクリーンショットと同じ位置で切れるか確かめる。100% や 200% でも起きるか調べる。ユーザーの Zed フォーク（`zed-gpui-windows-*`）で直っているかも確認する。
