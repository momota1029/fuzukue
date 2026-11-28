server-request-error = 操作を完了できませんでした: { $detail }
server-workspace-error = ワークスペースを開けませんでした: { $detail }
server-test-discovery-error = テストを検出できませんでした: { $detail }
server-git-error = Git の操作に失敗しました: { $detail }
server-thread-not-found = スレッドが見つからない
server-thread-running = スレッドは実行中
server-message-required = メッセージを入力する
server-too-many-attachments = 添付が多すぎます
server-no-comments = 送るコメントがない
server-codex-exited = Codex app-server が終了した
approval-run-command = コマンドの実行: { $command }{ $reason }
approval-change-files = ファイルの変更{ $reason }
approval-grant-turn-permissions = このターンに権限を付与: { $permissions }{ $reason }
approval-git-push = Git プッシュ · { $source } → { $destination }{ "\u000A" }{ $urls }
approval-worker-write = 変更予定外の { $tool } · { $path }{ "\u000A" }SHA-256: { $hash }{ "\u000A" }{ $diff }
approval-git-push-findings = Git プッシュ · { $source } → { $destination }{ "\u000A" }{ $urls }{ "\u000A\u000A" }まだリモートにないコミットで記録した個人情報の指摘:{ "\u000A" }{ $findings }
server-privacy-findings-committed = 個人情報の指摘が { $count } 件ある状態で { $commit } をコミットした。プッシュの前に次の場所を確かめる。{ "\u000A" }{ $locations }
approval-worker-tests = { $directory } でテストを実行{ "\u000A" }{ $commands }
approval-git-pull = Git 取り込み（早送りのみ） · { $source } → { $destination }{ "\u000A" }{ $urls }
approval-create-issue = { $repo } に Issue を作成{ "\u000A\u000A" }{ $title }{ "\u000A\u000A" }{ $body }
approval-mark-issue-ready = { $repo }#{ $number } に dot:ready を付ける
approval-merge-issue = { $repo }#{ $number }「{ $title }」の成果を取り込み、GitHub に結果をコメントして閉じる{ "\u000A\u000A" }{ $changes }{ "\u000A\u000A" }{ $summary }
server-inbox-error = 添付を保存できませんでした: { $detail }
server-building-pdf = PDF を作っている…
server-built-pdf = PDF を作った
server-job-cancelled = 止めた
server-review-running = 査読中…
server-review-superseded = 変更を取り込んだ
server-review-error = 査読に失敗しました: { $detail }
server-review-skipped = 査読を見送りました: { $detail }
server-compacting = 圧縮中…
server-compacted = 圧縮した
server-compaction-error = 圧縮に失敗しました: { $detail }
server-git-pushed = プッシュした
server-git-pulled = 取り込みが完了した
server-git-committed = コミットした · { $detail }
server-plan-done = 完了条件を満たした
server-plan-waiting = 確認したい点がある
server-plan-next = 次は { $detail }
server-plan-updated = 方針を更新: { $detail }
server-worker-question = 並行作業からの質問
server-front-desk-reply = 受付からの回答
server-default-model = { $detail } 既定
server-paused = 休憩中
server-claude-rate-limit = Claude が使用量の上限で止まった
server-rate-window = { $backend } の { $window } 枠が { $percent }
server-plan-update-error = 方針を更新できませんでした
server-goal-continuation-error = 目標を継続できませんでした
server-plan-review-error = 方針の見直し: { $detail }
server-tool-guard-desk = このターンでツールの呼び出しが上限を超えた。次のターンまで Fuzukue のツールを停止した。
server-tool-guard-worker = { $label } は上限を超えてもツールの呼び出しが続いたため停止した。作業ツリーと途中経過は残した。
server-front-desk-error = 受付への連絡: { $detail }
server-front-desk-switch-wait = 受付への連絡: 相手を替えるには Claude のターンの完了を待つ
server-claude-switch-wait = 相手を替えるには Claude のターンの完了を待つ
server-partner-switch-wait = 相談役を替えるにはターンの完了を待つ
server-codex-stopped = Codex: 止めた
server-thread-save-error = スレッドを保存できない: { $detail }
server-agent-usage-error = 作業役の集計: { $detail }
server-resume-error = 再開できない: { $detail }
server-auto-approval-error = 自動承認に失敗しました: { $detail }
server-invalid-message = 不正な要求: { $detail }
server-codex-error = Codex: { $detail }
server-full-approval-until = 完全承認モードは { $at } まで有効{ "\u000A" }確認なしで進む: シェル、ファイル編集、手元で動くツール、作業の委譲、コミットとパッチの取り込み（個人情報の指摘がある場合も含み、プッシュの前にもう一度示す）。{ "\u000A" }確認する: プッシュ、公開などの外部への書き込み、Web へのアクセス、履歴を失う Git 操作、管理者権限の要るインストール、サーバーの認証情報、判定できないコマンド、外部の入力で始まったターン。「毎回確認」と「計画」のスレッドは引き続き確認する。{ "\u000A" }例外: 自動承認にする前に、起動設定で承認を auto か never と明示して起動した作業役は、その設定のままで、確認しないことがある。{ "\u000A" }コマンドの判定で防げるのはうっかりした誤りだけで、作業は隔離されない。外部の入力が届いた後も、ファイル編集は続くことがある。
server-full-approval-until-disabled = 完全承認モードは停止するまで有効{ "\u000A" }確認なしで進む: シェル、ファイル編集、手元で動くツール、作業の委譲、コミットとパッチの取り込み（個人情報の指摘がある場合も含み、プッシュの前にもう一度示す）。{ "\u000A" }確認する: プッシュ、公開などの外部への書き込み、Web へのアクセス、履歴を失う Git 操作、管理者権限の要るインストール、サーバーの認証情報、判定できないコマンド、外部の入力で始まったターン。「毎回確認」と「計画」のスレッドは引き続き確認する。{ "\u000A" }例外: 自動承認にする前に、起動設定で承認を auto か never と明示して起動した作業役は、その設定のままで、確認しないことがある。{ "\u000A" }コマンドの判定で防げるのはうっかりした誤りだけで、作業は隔離されない。外部の入力が届いた後も、ファイル編集は続くことがある。
server-full-approval-off = 完全承認モードは無効{ "\u000A" }「すべて自動」にしたスレッドの作業は、引き続き確認なしで進む。

server-parallel-limit = 並行数の上限に達している（{ $running }/{ $limit } 件が実行中。このスレッドの設定: { $setting ->
    [normal] 標準
    [economy] 節約
   *[full] 全力
    }）
server-usage-hold = { $backend } の使用量がリセット前に尽きる見込みのため保留中（リセット: { $reset }。消費が緩めば先に開始する）
server-usage-parallel = { $backend } の使用量に余裕がないため並行数を制限中（{ $running }/{ $limit } 件が実行中）
server-parallel-start-delay = 次の作業の開始間隔を確保中（{ $running }/{ $limit } 件が実行中。このスレッドの設定: { $setting ->
    [normal] 標準
    [economy] 節約
   *[full] 全力
    }）
