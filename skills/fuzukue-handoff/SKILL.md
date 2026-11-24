---
name: fuzukue-handoff
description: Write a handoff note so a fuzukue thread can take over this session's work (goal, progress, decisions, remaining tasks as issue-sized items with acceptance criteria, running Codex workers, repository state, procedures). Use when the user asks to hand off, "引き継ぎを書いて", move this work into fuzukue, or stop this session at a good point.
---

# Handoff to fuzukue

Design: `cards/design/handoff.md` in the fuzukue repository. The receiving side is a new fuzukue thread in the same repository's workspace; its front desk reads this note first, then proposes issues from the remaining tasks and may adopt running Codex workers by thread ID.

## Where to write

`<repo>/.fuzukue/handoff/<YYYY-MM-DD>-<short-slug>.md` in the repository the work belongs to (create the directory). Do not commit it unless the user asks; tell the user the path. If several repositories are involved, write one note per repository and cross-link them.

Write the note in the user's working language (for this user: Japanese, plain written style — not the conversational tone). Prefer facts you verified in this session over memory: run the commands below rather than recalling.

## Gather first (run, do not recall)

- `git status --short`, `git branch --show-current`, `git worktree list`, `git log --oneline -15`, and for each worker branch `git log --oneline <base>..<branch>` and whether it is pushed (`git rev-parse --verify -q origin/<branch>`).
- Running Codex workers: `mcp__codex-agents__agent_list` / `agent_result` (thread ID, label, cwd, status, last message summary). Note which are still running and what each is expected to deliver.
- Your own transcript path: Claude Code `~/.claude/projects/<project-key>/<session-id>.jsonl` (the session id is in the environment or the newest jsonl in that directory); Codex thread IDs for workers.
- Test/verification state you actually observed (commands and results), not assumed.

## Template

```markdown
---
kind: handoff
from: <Claude Code session | Codex thread> <id>
transcript: <absolute path to the jsonl, or thread id>
repo: <absolute repo path>
date: <YYYY-MM-DD>
---

# <One-line title of the work>

## 目標
<What the work is for, and the definition of done for the whole effort.>

## 今どこまで来たか
- 済んだこと: <with commits / branches / files>
- 決めたこと: <decision — reason>
- 捨てた案: <idea — why it was dropped> (so the next agent does not retry it)
- 未確認のこと: <claims not yet verified>

## 残っている仕事
Each item is issue-sized: one worker, one branch, verifiable.

### 1. <title>
- 内容: <what to do, where (files, theorem names, modules)>
- 受け入れ条件: <checkable conditions; for Lean: "<theorem> builds without sorry; #print axioms shows only propext, Classical.choice, Quot.sound"; for code: the test command that must pass>
- 依存: <other items / branches it needs>
- 手がかり: <relevant files, prior attempts, worker reports>

## 動いている作業者
| スレッド ID | 担当 | worktree / ブランチ | 状態 | 期待する成果 |
|---|---|---|---|---|

## リポジトリの状態
- 統合用のブランチ: <name, tip sha, pushed?>
- worktree: <path — branch — clean/dirty>
- コミットしていない変更: <paths, and whether they matter>

## 手順と道具
- スキル: <names and paths, e.g. yulang-lean-harness>
- スクリプト: <verification, watch, stall detection — with exact commands>
- 監視: <how workers are watched, e.g. tools/codex-watch.sh <threadId>:<label>>

## 気をつけること
<Traps, environment quirks, rules the user insisted on.>
```

## Rules

- Be concrete: paths, branch names, SHAs, theorem/function names, exact commands. The reader has none of this session's context.
- Each remaining task must stand alone as an issue body: no "as discussed above".
- Do not invent acceptance criteria the user did not agree to; mark guesses as 「案」.
- Do not include secrets, tokens, or private personal details.
- Before finishing, re-read the note once as the receiver: could a fresh agent start item 1 using only this note and the repository? Fix gaps.
- Finish by telling the user the path and how many remaining tasks and running workers it lists.
