chat-agent-default-model = Agent default{ STATUS_SEPARATOR() }{ $backend }{ STATUS_SEPARATOR() }{ $model }
chat-agents = Agents
chat-allow = Allow
chat-applied-cards-changed = Applied (cards changed: { $cards })
chat-apply-findings-in-a-separate-thread = Apply findings in a separate thread
chat-apply-in-a-separate-thread = Apply in a separate thread
chat-applying-comments = Applying { $count ->
    [one] { $count } comment
   *[other] { $count } comments
    } in a separate thread
chat-approval-mode-applies-next-turn = Approval mode. Applies next turn
chat-approve-edits-automatically = Approve edits automatically
chat-approve-everything = Approve everything
chat-ask-every-time = Ask every time
chat-attach-files = Attach files
chat-attach-files-up-to-50-mib-each = Attach files (up to 50 MiB each)
chat-attached-deck = Attached to { $kind } “{ $title }”
chat-attachments-are-limited-to-50-mib-per-file = Attachments are limited to 50 MiB per file
chat-automatic-review = Automatic review
chat-automatically-allowed = Automatically allowed
chat-automatically-declined = Automatically declined
chat-worker-approval-selection = Worker: { $policy }
chat-worker-approval-help = Who decides eligible worker approvals in this workspace. Other requests always go to you.
chat-worker-approval-ask = Ask me
chat-worker-approval-advisor = Ask the advisor
chat-worker-approval-auto = Auto-approve
chat-awaiting-approval = Awaiting approval{ STATUS_SEPARATOR() }{ $summary }
chat-approval-pending = Awaiting approval…
chat-backend-model-and-reasoning-effort = Backend, model and reasoning effort
chat-cannot-open-the-file-picker = Couldn’t open the file picker: { $err }
chat-cannot-read-clipboard-image = Couldn’t read the clipboard image
chat-cannot-send-attachments = Couldn’t send attachments
chat-change-recipient = Change recipient
chat-choose-a-regular-file = Choose a regular file
chat-choose-attachments = Choose attachments
chat-claude-cli-default = Claude CLI default
chat-cli-default = CLI default
chat-cli-default-chat = CLI default
chat-codex-approval-mode-help = Codex starts in read-only mode with Ask every time. Actions that need approval appear in chat.
chat-collapse = Collapse
chat-connect-to-a-server-that-supports-attachments = Connect to a server that supports attachments
chat-connection-lost-select-the-attachment-again = Connection lost. Select the attachment again.
chat-context = Context { $used } / { $limit_label }
chat-context-usage = Context usage
chat-default = Default
chat-default-advisor = Default advisor
chat-default-cli-model = Default CLI model
chat-default-model = Default model
chat-default-worker = Default worker
chat-deny = Deny
chat-done = Done
chat-elapsed-minutes-seconds = { $minutes }m { $seconds }s
chat-elapsed-seconds = { $seconds }s
chat-ended = Ended
chat-enter-a-message = Enter a message
chat-expand = Expand
chat-extra-high = Extra high
chat-failed = Failed
chat-failure-details = Failure details
chat-worker-recovery = The server stopped before the worker finished.
chat-fallback-advisor-same-backend = Fallback advisor (same backend)
chat-fallback-default-worker = Fallback default worker
chat-fallback-model = Fallback model
chat-fallback-progress-summary = Fallback progress summary
chat-final-report = Final report
chat-worker-messages = Worker messages
chat-high = High
chat-input-output-tokens = { $input_tokens } input{ STATUS_SEPARATOR() }{ $output_tokens } output tokens
chat-jump-to-latest = Jump to latest
chat-loading-attachment = { $name }{ STATUS_SEPARATOR() }Loading
chat-low = Low
chat-math = Math
chat-maximum = Maximum
chat-medium = Medium
chat-model-and-reasoning-effort-apply-next-turn = Model and reasoning effort. Apply next turn
chat-model-applies-next-turn = Model. Applies next turn
chat-no-fallback-model = No fallback model
chat-no-output = No output
chat-no-usage-data-yet = No usage data yet
chat-none = None
chat-paper = Paper
chat-plan-only = Plan only
chat-planner = Planner
chat-plan-updated = Plan updated
chat-plan-updated-count = Plan updated ({ $count } times)
chat-plan-update-timeout = Plan update timed out (will retry on the next update)
chat-plan-update-timeout-count = Plan update timed out (will retry on the next update) ({ $count } times)
chat-plan-update-failed = Plan update failed (will retry on the next update)
chat-plan-update-failed-count = Plan update failed (will retry on the next update) ({ $count } times)
chat-plan-update-rejected = Plan update rejected (invalid output format)
chat-plan-update-rejected-count = Plan update rejected (invalid output format) ({ $count } times)
chat-goal-continuation-timeout = Goal continuation timed out
chat-goal-continuation-timeout-count = Goal continuation timed out ({ $count } times)
chat-goal-continuation-failed = Couldn’t continue the goal
chat-goal-continuation-failed-count = Couldn’t continue the goal ({ $count } times)
chat-progress-summary-model = Progress summary model
chat-reasoning-effort = Reasoning effort
chat-reasoning-effort-applies-next-turn = Reasoning effort. Applies next turn
chat-remove-attachment = Remove attachment
chat-remove-named-attachment = Remove { $name }
chat-research = Research
chat-resume-now = Resume now
chat-tool-running = Running
chat-tool-state-running = Running { $name }…
chat-save-as-draft = Save as draft
chat-save-findings-as-a-draft = Save findings as a draft
chat-saving = Saving…
chat-selected-recipient = Selected recipient
chat-type-a-message = Type a message
chat-send-held-for-update = Your message will be held until the update finishes
chat-send = Send
chat-send-ctrl-enter = Send (Ctrl+Enter)
chat-send-to-this-worker = Send to this task
chat-stop = Stop
chat-stop-reply = Stop reply
chat-stop-worker = Stop worker "{ $title }"
chat-thinking = Thinking…
chat-consulting = Consulting…
chat-to-advisor = To advisor
chat-to-worker = To parallel task
chat-toggle-details = { $action } { $label }
chat-tool-calls = { $calls ->
    [one] { $calls } tool call
   *[other] { $calls } tool calls
    }
chat-undo = { " " }(Undo)
chat-unknown-model = Unknown model
chat-unspecified = Unspecified
chat-up-to-attachments-per-message = Up to { $max_attachments } attachments per message
chat-unsupported-clipboard-image = This clipboard image format isn’t supported
chat-usage-turns = Reasoning: { $effort }{ STATUS_SEPARATOR() }{ $turns ->
    [one] { $turns } turn
   *[other] { $turns } turns
    }
chat-use-defaults = Use defaults
chat-use-shared-settings = Use shared settings
chat-waiting = Waiting
chat-worker = Parallel task
chat-working = Working
chat-writing = Writing…
chat-review-writing = Writing
chat-save-memo = Save as memo
chat-memo-saved = Saved as memo
chat-copy-message = Copy message
chat-copy-code = Copy
chat-code-copied = Copied
chat-quote-reply = Quote reply
chat-worker-tools = { $worker }: { $calls ->
    [one] { $calls } tool call
   *[other] { $calls } tool calls
    }{ STATUS_SEPARATOR() }{ $status }
chat-earlier-worker-tools = { $calls ->
    [one] { $calls } earlier tool call
   *[other] { $calls } earlier tool calls
    }
chat-tool-group-status = { $label }{ STATUS_SEPARATOR() }{ $status }
chat-tool-group-error = { $label }{ STATUS_SEPARATOR() }Errors
chat-rewrite = Rewrite
chat-decided-approvals = Approval decisions ({ $count })
chat-status-working = Working…
chat-status-waiting = Waiting…
chat-status-count = { $count } { $count ->
    [one] job
   *[other] jobs
    }
chat-status-workers = { $count } { $count ->
    [one] worker
   *[other] workers
    }
chat-status-details = { $status } Show details
chat-activity-details = Details
chat-activity-send = Send
chat-activity-full-message = Full message
chat-activity-last = Latest activity: { $activity }
chat-activity-elapsed = Elapsed: { $elapsed }
chat-attachment-enlarge = Enlarge { $name }
chat-activity-working = Working
chat-activity-background-commands =
    { $count ->
        [one] { $count } command running
       *[other] { $count } commands running
    }
chat-activity-command = Command
chat-activity-command-running = Running
chat-activity-command-completed = Completed
chat-activity-command-failed = Failed
chat-activity-command-stopped = Stopped
chat-activity-waiting = Waiting
chat-wait-usage = Usage limit
chat-wait-usage-resume = Usage limit (resumes in { $remaining })
chat-wait-update-check = Preparing the server update
chat-wait-update-build = Building the server update
chat-wait-update-test = Testing the server update
chat-wait-update-shot = Checking the server update UI
chat-wait-update-publish = Publishing the server update
chat-wait-update-wait = Waiting to switch servers
chat-wait-update-swap = Switching servers
chat-wait-update-canary = Server update preview awaiting approval
chat-wait-worker = Worker completion
chat-wait-test = Test completion
chat-wait-build = Build completion
chat-wait-queued-turn = Next front desk turn
chat-wait-queued-count = { $count } queued
chat-wait-goal = Next goal step
chat-goal-verification = Done{ STATUS_SEPARATOR() }please check ({ $reason })
chat-goal-paused = Paused
chat-wait-watch = Next watch event

## Composer commands

command-compact = Compact the conversation context
command-clear = Start a fresh session in this thread
command-new = Create a new thread
command-model = Show or set the conversation model
command-effort = Show or set the reasoning effort
command-archive = Archive this thread
command-pin = Pin this thread
command-goal = Show the current plan or revise it
command-pace = Show or set the goal’s parallel job limit
command-stop = Interrupt the current turn
command-help = List the available commands
command-unknown = Unsupported command. Use /help to see the list.
command-busy = Commands are unavailable during a turn. Use /stop to interrupt it.
command-unavailable = This command is unavailable for the current backend.
command-usage = Usage: { $usage }
command-queued = Compaction queued.
command-cleared = New session
command-archived = Thread archived.
command-pinned = Thread pinned.
command-stopped = Interrupt requested.
command-idle = No turn is running.
command-default = Default
command-value = { $command } — { $value }
command-no-goal = This thread has no plan.
command-plan = Goal: { $goal }
    Completion criteria: { $condition }
    Next: { $next }
command-failed = { $command } failed — { $detail }
command-compacted = Conversation context compacted.
command-compacted-counts = Conversation context compacted ({ $before } → { $after } tokens)
command-context-high = Context usage is above { $percent }%. Use /compact to reduce it.
chat-wait-compacting = Compacting…
chat-question-other = Other
chat-question-text-placeholder = Type your answer
chat-question-send = Send
chat-question-dismiss = Dismiss
chat-question-answered = Answered: { $answers }
chat-question-dismissed = Question dismissed
chat-question-cancelled = Question cancelled
