server-request-error = Couldn’t complete the operation: { $detail }
server-workspace-error = Couldn’t open the workspace: { $detail }
server-test-discovery-error = Couldn’t discover tests: { $detail }
server-git-error = Git operation failed: { $detail }
server-thread-not-found = Thread not found
server-thread-running = This thread is already running
server-message-required = Enter a message
server-too-many-attachments = Too many attachments
server-no-comments = No comments to send
server-codex-exited = Codex app-server exited
approval-run-command = Run command: { $command }{ $reason }
approval-change-files = Change files{ $reason }
approval-grant-turn-permissions = Grant permissions for this turn: { $permissions }{ $reason }
approval-git-push = Git push · { $source } → { $destination }{ "\u000A" }{ $urls }
approval-worker-write = { $tool } outside touches · { $path }{ "\u000A" }SHA-256: { $hash }{ "\u000A" }{ $diff }
approval-git-pull = Git pull (fast-forward only) · { $source } → { $destination }{ "\u000A" }{ $urls }
approval-create-issue = Create an issue in { $repo }{ "\u000A\u000A" }{ $title }{ "\u000A\u000A" }{ $body }
approval-mark-issue-ready = Add dot:ready to { $repo }#{ $number }
approval-merge-issue = Merge work for { $repo }#{ $number } “{ $title }”, post the result to GitHub, and close the issue{ "\u000A\u000A" }{ $changes }{ "\u000A\u000A" }{ $summary }
server-inbox-error = Couldn’t save the attachment: { $detail }
server-building-pdf = Building PDF…
server-built-pdf = PDF built
server-job-cancelled = Stopped
server-review-running = Reviewing…
server-review-superseded = Restarting review for the latest changes
server-review-error = Review failed: { $detail }
server-review-skipped = Review skipped: { $detail }
server-compacting = Compacting…
server-compacted = Session compacted
server-compaction-error = Compaction failed: { $detail }
server-git-pushed = Push complete
server-git-pulled = Pull complete
server-git-committed = Committed · { $detail }
server-plan-done = Completion criteria met
server-plan-waiting = Waiting for your input
server-plan-next = Next: { $detail }
server-plan-updated = Plan updated · { $detail }
server-worker-question = Question from a parallel task
server-front-desk-reply = Reply from front desk
server-default-model = { $detail } default
server-paused = Waiting to resume
server-claude-rate-limit = Claude reached its usage limit
server-rate-window = { $backend } usage for { $window }: { $percent }
server-plan-update-error = Couldn’t update the plan
server-goal-continuation-error = Couldn’t continue the goal
server-plan-review-error = Couldn’t review plan: { $detail }
server-tool-guard-desk = Too many tool calls in this turn. Further Fuzukue tool calls are blocked until the next turn.
server-tool-guard-worker = { $label } was stopped after continuing to call tools past the limit. Its worktree and partial report were kept.
server-front-desk-error = Couldn’t contact front desk: { $detail }
server-front-desk-switch-wait = Front desk: Wait for Claude’s turn to finish before switching partners
server-claude-switch-wait = Wait for Claude’s turn to finish before switching partners
server-partner-switch-wait = Wait for the current turn to finish before switching partners
server-codex-stopped = Codex stopped
server-thread-save-error = Couldn’t save thread: { $detail }
server-agent-usage-error = Couldn’t load agent usage: { $detail }
server-resume-error = Couldn’t resume: { $detail }
server-auto-approval-error = Automatic approval failed: { $detail }
server-invalid-message = Invalid request: { $detail }
server-codex-error = Codex error: { $detail }
server-full-approval-until = Full approval is on until { $at }
server-full-approval-until-disabled = Full approval is on until turned off
server-full-approval-off = Full approval is off

server-parallel-limit = Parallel limit reached ({ $running } of { $limit } running; this thread's setting: { $setting ->
    [normal] normal
    [economy] economy
   *[full] full
    })
server-parallel-start-delay = Waiting between job starts ({ $running } of { $limit } running; this thread's setting: { $setting ->
    [normal] normal
    [economy] economy
   *[full] full
    })
