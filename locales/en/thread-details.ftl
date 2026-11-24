thread-details-advisor-model = Advisor{ STATUS_SEPARATOR() }{ $model }
thread-details-approval-status = Approval: { $mode }{ STATUS_SEPARATOR() }{ $status }
thread-details-artifact-count = Artifacts ({ $count })
thread-details-attached-papers-and-research = Attached papers and research
thread-details-available-tools = Available tools
thread-details-cannot-save-thread-details-visibility = Couldn’t save thread details visibility: { $error }
thread-details-clear-long-term-goal = Clear long-term goal
thread-details-cli-default = CLI default
thread-details-close-thread-details = Close thread details
thread-details-compact-session = Compact session
thread-details-complete = Complete
thread-details-completed = Completed
thread-details-completion-criteria = Completion criteria
thread-details-context = Context
thread-details-context-percent = { $used } / { $window } tokens ({ $percent }%)
thread-details-current-usage-unavailable = Current usage unavailable
thread-details-default = Default
thread-details-default-model = Default model
thread-details-default-worker = Default worker
thread-details-deleted = { $label }{ STATUS_SEPARATOR() }Deleted
thread-details-input-cache = Input cache: { $read } read{ STATUS_SEPARATOR() }{ $written } written
thread-details-session-copy-tooltip = { $id }{ "\u000A" }Click to copy
thread-details-next-steps = Next steps
thread-details-no-usage-data-yet = No usage data yet
thread-details-none = None
thread-details-not-started = Not started
thread-details-open-thread-details = Open thread details
thread-details-partner = Partner
thread-details-paused = Paused
thread-details-paused-resume-time-unknown = Paused{ STATUS_SEPARATOR() }Resume time unknown
thread-details-paused-until = Paused{ STATUS_SEPARATOR() }Resumes in { $time }
thread-details-plan = Plan
thread-details-plan-history = Past history
thread-details-planner = Planner
thread-details-reasoning = Reasoning: { $effort }
thread-details-reasoning-effort = Reasoning: { $effort }
thread-details-requesting-compaction = Requesting compaction…
thread-details-review-plan = Review plan
thread-details-running = Running
thread-details-saved = Saved
thread-details-settings-for-this-thread = Settings for this thread
thread-details-subagent = Subagent
thread-details-tests = Tests
thread-details-throttled = Throttled
thread-details-to-verify = To verify
thread-details-tokens-limit-unavailable = { $used } tokens{ STATUS_SEPARATOR() }Limit unavailable
thread-details-tokens-until-auto-compaction = { $count } tokens until auto-compaction
thread-details-total-input-output-turns = Total: { $input_tokens } input{ STATUS_SEPARATOR() }{ $output_tokens } output{ STATUS_SEPARATOR() }{ $turns ->
    [one] { $turns } turn
   *[other] { $turns } turns
    }{ $cost }
thread-details-unsuccessful-approaches = Unsuccessful approaches
thread-details-waiting = Waiting
thread-details-waiting-for-worker-report = Waiting for worker report
thread-details-worker = Worker
thread-details-worker-running = Worker{ STATUS_SEPARATOR() }{ $model }{ STATUS_SEPARATOR() }Running
thread-details-pace-full = Full (unlimited)
thread-details-pace-normal = Normal (up to { $limit })
thread-details-pace-economy = Economy (up to { $limit })
thread-details-goal-title = Goal: { $title }
thread-details-goal-steps = Progress: { $done }/{ $total } steps
thread-details-goal-pace = Parallel jobs: { $pace }
thread-details-goal-pace-unset = Unset
thread-details-goal-pace-tooltip = Maximum jobs running at once. Economy also slows plan updates and saves usage.
thread-details-goal-status = Status: { $state }
thread-details-goal-ready = Ready to proceed
thread-details-goal-waiting = Waiting
thread-details-goal-verification = Done (verification pending)
thread-details-step-blocked = Needs confirmation: { $reason }
thread-details-pause-goal = Pause goal
thread-details-goal-marker = Has a goal: { $title }
thread-details-goal-attention-blocked = { $count ->
    [one] Step waiting on you ({ $count })
   *[other] Steps waiting on you ({ $count })
    }
thread-details-goal-attention-question = Question pending
thread-details-goal-attention-waiting = Awaiting input

command-parallel-setting = Parallel jobs: { $setting ->
    [full] { thread-details-pace-full }
    [normal] { thread-details-pace-normal }
    [economy] { thread-details-pace-economy }
   *[unset] { thread-details-goal-pace-unset }
    }
