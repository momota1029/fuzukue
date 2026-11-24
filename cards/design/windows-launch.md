---
id: windows-launch
kind: design
title: Windows クライアントから WSL のサーバーを起動する
status: accepted
links:
  - {to: server-client-split, rel: refines}
  - {to: appearance, rel: refines}
files: [crates/fuzukue/src/conn.rs, crates/fuzukue/src/server.rs, crates/fuzukue-view/src/server.rs, scripts/setup-windows.ps1, docs/windows.md]
---

> 「guiをwindowsでcloneしてbuildしたらserverがWSLで動くみたいなことってできるんですかね」

- WSL2 と Windows のループバック接続を利用する。起動時に WebSocket を試し、ポートが接続を拒否した場合に限り、設定で選んだ WSL またはローカルのサーバーを起動する。既定の外部モードは接続だけを行う。
- WSL では `--exec /usr/bin/env FUZUKUE_ADDR=…` にサーバーとプロジェクトの引数を渡す。接続の再試行は約 60 秒を上限とし、失敗時は接続状態とサーバーの stderr 末尾を表示する。
- 通常終了と再起動ではクライアントが所有するプロセスを停止する。WSL の Linux プロセスは Windows 側の relay 終了後にも残り得るため、対象ディストリビューション内でユーザー、実行ファイル、プロジェクト、起動 ID を照合して停止する。
- セットアップは WSL のホームでサーバーをビルドし、Windows のローカルドライブでクライアントをビルドする。待ち受けや起動の設定は外観と同じ設定ファイルに保存する。
