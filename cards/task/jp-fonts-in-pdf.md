---
id: jp-fonts-in-pdf
kind: task
title: 和文フォントが埋め込まれていない PDF の日本語が出ない
status: done
links:
  - {to: artifact-views, rel: see}
---
`gpui-pdf`（hayro）はシステムのフォントを使わず、埋め込まれていない CID フォント（Ryumin-Light、GothicBBB-Medium）を描けない。
hayro 0.8 は埋め込まれていないフォントの代わりを受け付ける口がまだない（`FontQuery::Fallback` は未実装と明記）。

2026-11-19 の時点で TeX の和文フォントは `noEmbed`（`kanji-config-updmap-sys status`）。原ノ味は入っている。
`kanji-config-updmap-user haranoaji` で埋め込むようにすれば、作り直した PDF から日本語が出る。TeX 環境の設定なので、ユーザーが決める。

2026-11-19 に解決: ユーザーの了承で `kanji-config-updmap-user haranoaji` を実行。試しの PDF で原ノ味の明朝・ゴシックが埋め込まれることを確かめた。既存の PDF は次のビルドから。
