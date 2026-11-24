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
- `Hello.markdown_components` は省略可能な `[{tag, version}]`。省略時は対応タグが空で、古いサーバーは項目を読み飛ばす。部品は通常の Markdown フェンスで配信する。 [XML 部品](../../docs/xml-components.md)
- v64 で選択したスレッドの履歴取得を追加。合意値 62 以上で `Hello.thread_history` と `Hello.selection_scoped_transcripts` がともに真なら、対応サーバーは `Welcome.selection_scoped_transcripts` を返す。一覧と稼働状態を `ThreadSync` で区切り、選択したスレッドの `GetThreadTail` にだけ `ThreadTail` を返す。再接続では `epoch`・`resume_from`・`cached_sessions` でキャッシュを照合する。Android は R-18 まで従来の履歴配信を使う。 [選択したスレッドの履歴取得](../../docs/thread-transcripts.md)
- v65 はスレッドのフォルダー許可を追加する。ネイティブ接続で `ListFolderGrants { session, audit_before? }` に `FolderGrants { session, grants, audit, audit_before? }` を返す。追加・取り消しは `AddFolderGrant { session, path, expiry }` と `RevokeFolderGrant { session, grant_id }`。変更時は `FolderGrantsChanged { session }` を配信する。読み取り依頼は `approval_requested` の `folder-access-` で始まる `request_id` で識別する。`DecideFolderAccess { request_id, expiry? }` で期間を指定して許可し、期間の省略または null で拒否する。判断は既存の `approval_resolved` で通知する。v64 以下にはフォルダー関連イベントを送らない。 [フォルダーの読み取り許可](../../docs/folder-grants.md)
