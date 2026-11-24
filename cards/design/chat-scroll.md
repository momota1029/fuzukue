---
id: chat-scroll
kind: design
title: チャットのスクロール（指・タッチパッド対策）
status: accepted
links:
  - {to: server-client-split, rel: requires}
files: [crates/fuzukue/src/main.rs]
---
gpui-component の `MessageScroller` を使う。一番下にいるときだけ追従し、上に戻したら追従を外して
「最新へ」ボタンを出す。項目が伸びたら `remeasure_items` で再計測する。

未決: 慣性スクロール中にプログラム側から位置を動かさないこと、アンカー位置を保つこと。
ユーザーの `~/rust/gpui-filer` と Zed フォーク（`zed-gpui-windows-touch*`）に Windows のタッチ処理の作業がある。
`gpui-pre` 0.3.12 にそれが入っているかは未確認。入っていなければ、フォークに寄せるか、移植する。

タッチ入力でも会話をスクロールできるようにする。
