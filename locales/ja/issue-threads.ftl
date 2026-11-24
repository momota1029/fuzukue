github-link = Issue に紐づける
github-unlink = GitHub の紐づけを外す
github-enter-reference = 番号か URL を入力
github-reference = Issue または PR の番号か URL
github-policy = GitHub の応対方針
github-policy-notify = 知らせるだけ
github-policy-ask = 受付が応対
github-policy-allow = 受付に任せる
github-open = 開いている
github-closed = 閉じた
github-merged = マージ済み
github-reopened = 再開した
github-comment = コメント
github-review = レビュー
github-review-comment = レビューへのコメント
github-labeled = ラベルを追加
github-unlabeled = ラベルを削除
github-check = CI チェックの更新
github-reply = GitHub に返信
github-posting = 投稿中…
github-reply-posted = GitHub に返信を投稿した
github-linked-thread = 紐づいたスレッドを開く · { $title }
github-waiting = GitHub のレート制限で監視を休止中。再試行まで { $time }。
github-unread = GitHub { $count }
github-batch = GitHub の更新（{ $count } 件）
github-approve-comment = { $item } に次のコメントを投稿しますか？
    { $body }
github-approve-labels = { $item } のラベルを変更しますか？
    追加: { $add }
    削除: { $remove }
github-approve-close = GitHub Issue { $item } を閉じますか？
    理由: { $reason ->
        [completed] 完了
       *[not_planned] 対応予定なし
    }{ $has_comment ->
        [yes] { "\u000A\u000A" }クローズ時のコメント:{ "\u000A" }{ $comment }
       *[no] { "" }
    }
github-reply-disconnected = GitHub の投稿結果が届く前に接続が切れた。
github-respond = 対応する
github-respond-all = まとめて対応する
github-author-owner = あなた
github-author-agent = エージェントの報告
github-author-other = その他の投稿者
github-full = 全文
github-more = その他の操作
github-issue-opened = 新しい Issue · { $title }
github-issue-repo-format = Issue のリポジトリは owner/name で指定する。
github-issue-repo-denied = Issue のリポジトリは [github].issue_repos の許可一覧に含まれていない。
github-approve-tracking-issue = { $repo } に from-fuzukue ラベル付きで Issue を作成{ "\u000A\u000A" }{ $title }{ "\u000A\u000A" }{ $body }
github-intake-set = このスレッドを Issue の受付にする · { $repo }
github-intake-remove = 受付を外す · { $repo }
github-intake-badge = 受付
github-intake-badge-repo = { $repo } の Issue 受付
github-intake-move = Issue の受付を移す
github-intake-move-question = { $repo } の Issue 受付を「{ $title }」からこのスレッドへ移す？
github-intake-open = この Issue のスレッドを開く
github-intake-back = Issue の受付に戻る
github-intake-issue-opened = 新しい Issue
github-intake-pr-opened = 新しい PR
