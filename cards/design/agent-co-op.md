---
id: agent-co-op
kind: design
title: Claude が呼んだ Codex を同じチャット欄で動かす
status: accepted
links:
  - {to: agent-backends, rel: requires}
  - {to: protocol, rel: requires}
files: [crates/secretaire/src/codex.rs, crates/secretaire/src/mcp.rs]
---
サーバーが `codex app-server` を自分で持ち、その操作を MCP ツールとして Claude に渡す。
Claude が Codex を呼ぶと、サーバーはそのスレッドのイベント（計画、コマンド、差分、承認待ち）を購読し、
`Actor::Codex { parent_tool_use }` をつけて同じタイムラインに流す。
承認要求は `ApprovalRequested` / `Resolve` でクライアントのボタンから答える。

逆向き（Codex が主役で Claude を呼ぶ）も同じ仕組みで扱う（[[agent-backends]]）。

決定（2026-11-19、ユーザー）: 「ClaudeがCodexを呼んだときにちゃんと共闘する（同じチャット欄でちゃんとエージェントが動く）ようにもしたい」

実装（2026-11-19）:
- サーバーは `codex app-server` を最初に要るときに 1 つ起動する。スレッドは `workspace-write`、承認は `on-request`。
- Claude には `--mcp-config` で `http://<addr>/mcp?session=<id>` を渡し、`codex_run` を許可なしで呼べるようにする。
  呼び出しの親子は、Claude の stream-json に出る tool_use の ID を呼び出し順に積んで対応づける。
- 入力欄で `/codex ...` と書くと、Claude を通さず直接 Codex と話す。
- 確かめた: Claude → codex_run → Codex（コマンド実行）→ Claude の往復（約 9 秒）、入れ子の表示。
- 確かめていない: 承認要求のボタン（Codex が承認を求める場面を作れていない）、長いターン、中断。
