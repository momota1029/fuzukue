github-link = Link to an issue
github-unlink = Remove GitHub link
github-enter-reference = Enter a number or URL
github-reference = Issue or pull request number or URL
github-policy = GitHub activity policy
github-policy-notify = Notify only
github-policy-ask = Front desk asks first
github-policy-allow = Let the front desk respond
github-open = Open
github-closed = Closed
github-merged = Merged
github-reopened = Reopened
github-comment = Commented
github-review = Reviewed
github-review-comment = Commented on a review
github-labeled = Added label
github-unlabeled = Removed label
github-check = CI check updated
github-reply = Reply on GitHub
github-posting = Posting…
github-reply-posted = Reply posted on GitHub
github-linked-thread = Open linked thread · { $title }
github-waiting = GitHub polling paused (rate limit). Retry in { $time }.
github-unread = GitHub { $count }
github-batch = GitHub activity ({ $count })
github-approve-comment = Post this GitHub comment on { $item }?
    { $body }
github-approve-labels = Change labels on { $item }?
    Add: { $add }
    Remove: { $remove }
github-approve-close = Close GitHub issue { $item }?
    Reason: { $reason ->
        [completed] Completed
       *[not_planned] Not planned
    }{ $has_comment ->
        [yes] { "\u000A\u000A" }Closing comment:{ "\u000A" }{ $comment }
       *[no] { "" }
    }
github-reply-disconnected = Connection lost before GitHub confirmed the reply.
github-respond = Respond
github-respond-all = Respond to unread updates
github-author-owner = You
github-author-agent = Agent report
github-author-other = Other authors
github-full = Full text
github-more = More actions
github-issue-opened = New issue · { $title }
github-issue-repo-format = Specify the issue repository as owner/name.
github-issue-repo-denied = The issue repository is outside the [github].issue_repos allow-list.
github-approve-tracking-issue = Create an issue in { $repo } with the from-fuzukue label?{ "\u000A\u000A" }{ $title }{ "\u000A\u000A" }{ $body }
github-intake-set = Use this thread for issue intake · { $repo }
github-intake-remove = Remove issue intake · { $repo }
github-intake-badge = Intake
github-intake-badge-repo = Issue intake for { $repo }
github-intake-move = Move issue intake
github-intake-move-question = Move issue intake for { $repo } from “{ $title }” to this thread?
github-intake-open = Open this issue's thread
github-intake-back = Back to issue intake
github-intake-issue-opened = Issue opened
github-intake-pr-opened = Pull request opened
