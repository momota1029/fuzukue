---
id: agent-backends
kind: design
title: Claude と Codex を対等なエージェントとして扱う
status: accepted
links:
  - {to: protocol, rel: requires}
  - {to: agent-co-op, rel: see}
files: [crates/secretaire/src/claude.rs]
---
サーバーは「エージェントの裏側」を一つの形に揃える。どちらもセッションの主役になれ、もう一方を部下として呼べる。

- Claude: `claude -p --input-format stream-json --output-format stream-json`（実装済み）。部分出力は `--include-partial-messages`。
- Codex: `codex app-server`（JSON-RPC）。スレッド、ターン、承認要求、差分、使用量を受け取れる。

Claude Code と Codex を選べるようにする。

決定（2026-11-19、ユーザー）: この提案について「正直それでいいと思います」

権限（2026-11-19）:
- Claude は `--permission-mode acceptEdits`（作業ディレクトリの中の編集は自動で許す）と `--permission-prompt-tool stdio` で起動する。
  `git` / `gh` / `cargo` と `codex_run` は許可済み。それ以外で許可が要るものは `control_request`（`can_use_tool`）で
  サーバーに来て、Codex の承認要求と同じ「許可／拒否」ボタンで答える。
- Codex は `workspace-write` と `approvalPolicy: on-request`。外に出る操作は承認要求になる。
- どちらも「作業ディレクトリの中は任せ、外に出るときは人が決める」で揃えている。

決定（2026-11-19、ユーザー）:「正直権限についてはデフォで許可でもいい気はする」
→ 承認は既定で自動で許可する（`FUZUKUE_APPROVALS=ask` で人が答える形に戻る）。許可したものはタイムラインの承認の行として残る。
