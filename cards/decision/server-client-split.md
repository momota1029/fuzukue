---
id: server-client-split
kind: decision
title: Ubuntu にサーバー、Windows に GPUI クライアント
status: accepted
links:
  - {to: workbench, rel: implements}
  - {to: protocol, rel: see}
files: [crates/secretaire/, crates/fuzukue/]
---
プロジェクトのファイル・ビルド・エージェントはすべて Ubuntu (WSL2) 側の `secretaire` が持つ。
Windows 側の `fuzukue`（GPUI）は画面と入力だけを持つ。WSL2 の localhost 転送で `127.0.0.1:7878` に届く。

クライアントは Windows の cargo でビルドする（`CARGO_TARGET_DIR=%LOCALAPPDATA%\fuzukue\target`）。
GPUI は `gpui-kit` 0.10（`gpui-pre` 0.3.12）を使う。

決定（2026-11-19、ユーザー）: 「最初からUbuntu上にサーバみたいなものを立ち上げて，WindowsからアクセスしてGUIはGPUIでしっかり」
