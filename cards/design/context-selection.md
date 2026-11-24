---
id: context-selection
kind: design
title: 何を読むべきかをカードのグラフで決める
status: accepted
links:
  - {to: card-model, rel: requires}
---
作業（会話、Issue、dot への依頼）は入口カードを持つ。読むべきものは機械的に決める。

1. 入口カードから `requires` / `refines` / `implements` を辿った閉包を、依存が先の順で並べる（`fuzukue-cards context`）。
2. 閉包のカードが `files` で指すファイルを添える。
3. 編集するファイルが決まっていれば、そのファイルを指すカードを逆引きして足す（`fuzukue-cards files`）。

`see` / `mentions` は辿らない。広げすぎないための線引き。
エージェントは作業の終わりに、触ったファイルとカードのリンクを更新する（リンク切れは `check` が落とす）。

決定（2026-11-19、ユーザー）: この提案について「正直それでいいと思います」
