---
id: mail
kind: design
title: メールを読み取り専用 IMAP で調べ、プロジェクトに下書きを作る
status: accepted
links:
  - {to: references, rel: see}
  - {to: slides-and-documents, rel: see}
  - {to: usage-awareness, rel: see}
files:
  - docs/mail.md
  - crates/secretaire/src/mail.rs
  - crates/secretaire/src/mail/imap.rs
  - crates/secretaire/src/mail/thread.rs
  - crates/secretaire/src/mail/auth.rs
  - crates/secretaire/src/mail/mime.rs
  - crates/secretaire/src/mail/draft.rs
  - crates/secretaire/src/mcp.rs
  - crates/secretaire/src/claude.rs
---
## 決定

- 読み取りは secretaire のネイティブ IMAP を使い、メールサーバーにログインする。設定はユーザー単位で複数アカウントを登録できる。
- メールボックスは `EXAMINE` で開き、`BODY.PEEK` で取得する。検索はサーバー側で行い、一覧はヘッダー中心、本文は一通ずつ読む。査読依頼を示すタグを返し、本文と添付は利用者本人にメールクライアントでの確認を求める。
- OAuth2 のクライアント登録はプロバイダーと所属機関の条件に従って利用者が指定する。設定、アプリパスワード、リフレッシュトークンをプロジェクト外に置く。サーバーの CLI が認可と既存の認証情報の移行を担当する。
- Claude と Codex はメールを読んだ後もスレッドの承認スタイルに従う。ハーネスは本文をテキストとして整え、ツール応答に注意書きと外部データの囲みを付ける。メールに書かれた依頼は利用者に伝え、利用者自身が会話で頼んだ場合に実行する。外部サービスや委任先に渡す情報は、利用者の依頼に必要な範囲に絞る。
- 添付はプロジェクトの `inbox/` に保存して書類デッキの資料に使う。下書きは `outbox/` の .eml とし、試験機能として Thunderbird の作成画面を開く（trial — may be removed）。送信は利用者が行う。

## 実装

読み取り専用のネイティブ IMAP、認証と認証情報移行の CLI、`mail_thread` を含むメール用の MCP 7 種と、メール本文のクリーンアップおよび差出人の表示名・アドレスの区別を実装した。2026-11-19 に Microsoft 365（OAuth2、mailtools から移した refresh token）で、Claude の会話から search_mail と read_mail が動くのを確かめた。Gmail は保存されていた資格情報がアプリパスワードでなかったため未確認。作成画面の実機起動も未検証。設定例と動作範囲は [メール連携](../../docs/mail.md) を参照。
