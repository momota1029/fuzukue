---
id: remote-access
kind: design
title: 別の PC から Tailscale か LAN で繋ぐ（鍵つき、MCP は手元だけ）
status: accepted
links:
  - {to: server-client-split, rel: refines}
  - {to: mail, rel: see}
---
ユーザー（2026-11-19）:「遠隔接続もできるようにしたいんですよね．」「一応PCを考えてますね．論文の編集ですから．Tailscale(かLAN内ip)を使う想定です．」

ユーザー（2026-11-19）:「ブラウザは嫌だなあ．重いので．なんのためにGUI作ってもらったかわからない」

## 実装

- Windows / macOS の既存 GPUI クライアントから、Tailscale IP または HTTPS プロキシに接続する。`[server]` の `mode = "remote"` と `url` を設定し、必要なら `token` を追加する。設定ペインではトークンを伏せ字にし、WebSocket とファイル取得に同じ認証を使う。
- サーバーの既定は `127.0.0.1:7878`。`FUZUKUE_ADDR` のカンマ区切り、または `server.toml` の `[remote].listen` で手元と Tailscale IP など複数の待ち受けを設定できる。
- ループバックは従来どおり。`/mcp` はループバック専用。それ以外の経路では、Tailscale の IP 範囲に属する接続元を `tailscale whois` で照会してログイン名を許可リストと照合するか、トークンを検証する。
- 共有トークンは既定で初回起動時にプロジェクトの状態フォルダに権限 `0600` で作り、`secretaire token [--rotate]` で表示・再発行する。比較は時間一定。Bearer ヘッダーを使い、ヘッダーを設定できないクライアント向けに WebSocket のアップグレードだけ `?token=` にも対応する。
- WSL 内で Tailscale が動くこの環境では WSL の Tailscale IP を使う。設定手順と LAN・ファイアウォールの注意点は [docs/remote.md](../../docs/remote.md) にまとめる。

## Tailscale なしで使う場合

- 推奨は引き続き Tailscale。外出先のスマートフォンなどでは HTTPS を終端する信頼済みプロキシ経由で接続できる。tailnet 外では HTTPS とトークンを必須にする。
- 端末ごとにトークンを発行・失効できる。状態ディレクトリにはソルト付きハッシュを保存し、端末を失効させると接続済みの WebSocket も切断する。共有トークンは互換用に残し、設定で無効にできる。
- プロキシとローカル操作の待ち受けを分け、`/mcp` は直接のループバック接続に限る。

## 未決
- 同じサーバーに複数のクライアントが同時に繋いだときの、入力欄の競合（今は全部同じタイムラインを見る）。
