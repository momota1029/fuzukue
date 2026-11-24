---
id: windows-right-edge
kind: task
title: Windows（表示倍率 250%）でウィンドウ右端の約 29px が描かれない
status: open
links:
  - {to: server-client-split, rel: see}
---
2026-11-19 の確認: 画面 3840x2160、DPI 240（250%）。`GetClientRect` は 3500x2108（= 1400 論理px でレイアウトどおり）なのに、
描かれた絵は client 領域の右端の約 72 物理px（約 29 論理px）が黒く、そこにかかる要素が切れる。
レイアウト側（左 260 + 中央 + 右 45%）は client 幅に収まっているので、GPUI の Windows の描画（非整数倍率でのスワップチェーンの大きさ）を疑う。

確かめること: スクリーンショットではなく実画面でも切れているか（ユーザーに見てもらう）、100% や 200% でも起きるか、
ユーザーの Zed フォーク（`zed-gpui-windows-*`）で直っているか。
