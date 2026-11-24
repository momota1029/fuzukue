---
id: protocol
kind: spec
title: サーバーとクライアントのワイヤ形式
status: accepted
links:
  - {to: server-client-split, rel: requires}
files: [crates/fuzukue-proto/src/lib.rs]
---
- WebSocket のテキストフレーム 1 通 = JSON 1 メッセージ。
- サーバーのイベントは `Envelope { seq, event }`。`seq` は単調増加。
- 接続直後に `Hello { resume_from }`。サーバーは backlog から取りこぼし分を送る。切断しても会話は失われない。
- PDF などの大きいファイルは WebSocket に流さず `GET /file/<path>`。プロジェクトの外は 404。
- 発言者は `Actor`。Codex は `parent_tool_use` を持ち、呼び出した Claude のツール呼び出しの下に表示する（[[agent-co-op]]）。
- v17 の `LookupSymbol { request, lookup }` は記号の定義候補や参照を検索する。対応する `SymbolResults { request, results }` は要求元の接続に返す。
- v59 の `Notify { session, kind, title? }` はライブ接続専用。エンベロープの `workspace` が通知元を示し、現在選択中のワークスペース以外からも届く。履歴への記録、連番、再送はなく、表示と抑制はクライアントが決める（[[notifications]]）。
- v62 の履歴ページ取得の仕様は [選択したスレッドの履歴取得](../../docs/thread-transcripts.md) を参照。
- v63 の `ReviewFindings { deck, outcome }` は、接続が表示するデッキの指摘台帳のスナップショットまたは読み込みエラーを返す。初回の接続、デッキの変更、表示中のデッキの `review-findings.toml` の変更で送る。v62 以下を交渉した接続には配信しない。 [指摘台帳](../../docs/review-findings.md)
- v64 で選択したスレッドの履歴取得を追加。合意値 62 以上で `Hello.thread_history` と `Hello.selection_scoped_transcripts` がともに真なら、対応サーバーは `Welcome.selection_scoped_transcripts` を返す。一覧と稼働状態を `ThreadSync` で区切り、選択したスレッドの `GetThreadTail` にだけ `ThreadTail` を返す。再接続では `epoch`・`resume_from`・`cached_sessions` でキャッシュを照合する。Android は R-18 まで従来の履歴配信を使う。 [選択したスレッドの履歴取得](../../docs/thread-transcripts.md)
