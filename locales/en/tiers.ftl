tiers-details = Details
tiers-light = Light
tiers-standard = Standard
tiers-heavy = Heavy
tiers-role = Role: { $role }
tiers-selection = { $backend }{ STATUS_SEPARATOR() }{ $model }{ STATUS_SEPARATOR() }Effort: { $effort }
tiers-decision = { $tier }{ STATUS_SEPARATOR() }{ $reason }
tiers-attempt = { $tier }{ STATUS_SEPARATOR() }{ $outcome }{ STATUS_SEPARATOR() }{ $selection }{ STATUS_SEPARATOR() }{ $tokens } tokens{ STATUS_SEPARATOR() }{ $cost }{ STATUS_SEPARATOR() }{ $duration }
tiers-escalated = Escalated: { $reason }
tiers-duration = Duration: { $duration }
tiers-target-job = Send to this job (clear)
tiers-stalled = Stalled
tiers-review-heading = { $name }{ STATUS_SEPARATOR() }{ $deck }{ STATUS_SEPARATOR() }{ $state ->
    [running] In progress
    [failed] Failed
   *[waiting] Finished
}
tiers-stats = { $tier }{ STATUS_SEPARATOR() }{ $count } attempts{ STATUS_SEPARATOR() }{ $rate } success{ STATUS_SEPARATOR() }{ $cost }{ STATUS_SEPARATOR() }{ $weighted } weighted tokens
tiers-cost = Cost: { $cost }
tiers-usage-totals = Tier usage{ STATUS_SEPARATOR() }Last 30 days
