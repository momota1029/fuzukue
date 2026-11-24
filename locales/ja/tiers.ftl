tiers-details = 詳細
tiers-light = 軽量
tiers-standard = 標準
tiers-heavy = 重量
tiers-role = 役割: { $role }
tiers-selection = { $backend }{ STATUS_SEPARATOR() }{ $model }{ STATUS_SEPARATOR() }思考レベル: { $effort }
tiers-decision = { $tier }{ STATUS_SEPARATOR() }{ $reason }
tiers-attempt = { $tier }{ STATUS_SEPARATOR() }{ $outcome }{ STATUS_SEPARATOR() }{ $selection }{ STATUS_SEPARATOR() }{ $tokens } トークン{ STATUS_SEPARATOR() }{ $cost }{ STATUS_SEPARATOR() }{ $duration }
tiers-escalated = 段階を上げた理由: { $reason }
tiers-duration = 所要時間: { $duration }
tiers-target-job = この作業に送信（解除）
tiers-stalled = 停滞
tiers-review-heading = { $name }{ STATUS_SEPARATOR() }{ $deck }{ STATUS_SEPARATOR() }{ $state ->
    [running] 進行中
    [failed] 失敗
   *[waiting] 完了
}
tiers-stats = { $tier }{ STATUS_SEPARATOR() }試行 { $count } 回{ STATUS_SEPARATOR() }成功率 { $rate }{ STATUS_SEPARATOR() }{ $cost }{ STATUS_SEPARATOR() }重み付きトークン { $weighted }
tiers-cost = 費用: { $cost }
tiers-usage-totals = 段階ごとの実行{ STATUS_SEPARATOR() }過去30日間
