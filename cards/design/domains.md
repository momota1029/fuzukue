---
id: domains
kind: design
title: 論文、本、言語実装を「デッキ＋ビュー＋ビルド」の差で扱う
status: accepted
links:
  - {to: card-model, rel: requires}
  - {to: artifact-views, rel: see}
---
核は共通（チャット、カード、リンク、Issue、使用量）。領域ごとの差は 3 つだけにする。

| 領域 | カードの種類 | ビルド | 右のビュー |
|---|---|---|---|
| 論文（`~/research`） | definition / theorem / proof / expl | upLaTeX、synctex | PDF |
| 本 | 論文と同じ＋章・節の構成 | 同上（構成ファイルが大きいだけ） | PDF |
| 言語実装（`yulang`） | design / spec / task | cargo、Lean | テスト結果、Lean のゴール、差分 |
| fuzukue 自身 | design / spec / task | cargo | 差分、スクリーンショット |

順番は「論文に専念」ではなく、「fuzukue 自身の設計カードで核を固め（今）→ 論文で右ビューと
ビルドを作る → 本と yulang は種類とビューを足すだけ」。
gpui-filer は領域ではなく、GPUI のタッチ・スクロール対策の知見の出所（[[chat-scroll]]）。

決定（2026-11-19、ユーザー）: この提案について「正直それでいいと思います」
