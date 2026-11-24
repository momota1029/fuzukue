github-report-menu = 不具合を報告
github-report-settings = GitHub 連携
github-report-connected = { $login } として連携中
github-report-disconnected = GitHub アカウントは未連携
github-report-disconnect = 解除
github-report-starting = GitHub に確認コードを要求中…
github-report-copy-code = コードをコピー
github-report-repo = 送信先: { $repo }
github-report-no-repo = このワークスペースには GitHub の報告先が設定されていません。
github-report-preview = 題名と本文を確認してください。表示されている内容をそのまま GitHub に送ります。
github-report-title = 題名
github-report-title-prefix = 不具合:
github-report-body = 本文
github-report-send = 送る
github-report-sending = 送信中…
github-report-sent = Issue を作成しました
github-report-browser = ブラウザーで開く
github-report-long-body = 下書きが長いため、リンクに入りきらなかった項目をコピーし、GitHub のフォームに貼り付けてください。
github-report-copy-body = 本文をコピー
github-report-copy-title = 題名をコピー
github-report-disabled = このサーバーでは GitHub 連携を利用できません。
github-report-loading = GitHub 連携の状態を確認中…
github-report-config-error = サーバーが GitHub の設定を読み込めませんでした。
github-report-storage-error = サーバーが GitHub の認証情報を読み込み、保存、または削除できませんでした。
github-report-offline = 接続が切れました。報告の送信中だった場合は、再送する前に GitHub を確認してください。
github-report-rejected = GitHub が要求を受け付けませんでした（HTTP { $code }）。
github-report-response-error = GitHub から予期しない応答がありました。
github-report-denied = GitHub で連携が拒否されました。
github-report-expired = GitHub の確認コードが期限切れです。連携をやり直すと新しいコードが発行されます。
github-report-cancelled = GitHub の操作を中止しました。
github-report-already-connected = すでに GitHub アカウントが連携されています。連携状態を更新します。
github-report-not-connected = fuzukue から送るには GitHub アカウントを連携してください。
github-report-server-required = アプリから報告を送るには fuzukue サーバーに接続してください。
github-report-close-dialog = 開いているダイアログを閉じてから、不具合として報告してください。
github-report-screenshot-title = 画面写真からの報告
github-report-screenshot-attach = PNG を保存し、GitHub の Issue に添付してください。
github-report-save-screenshot = 画面写真を保存…
github-report-screenshot-saved = 画面写真を { $path } に保存しました。
github-report-screenshot-save-error = 画面写真を保存できませんでした（{ $error }）。
github-report-send-without-screenshot = 画像なしで送る
github-report-account-changed = 連携しているアカウントが変わりました。送信し直す前に新しい送信者を確認してください。
github-report-repo-denied = このワークスペースからこのリポジトリに報告できなくなりました。
github-report-empty = 報告の題名と本文を入力してください。
github-report-unknown = 不明
github-report-kind-research = 研究
github-report-kind-paper = 論文
github-report-kind-software = ソフトウェア
github-report-kind-empty = 空
github-report-template =
    { "**公開前に**" } 認証情報や非公開の内容は伏せてください。

    { "##" } 何をしたか

    <!-- 操作と入力 -->

    { "##" } 起きたこと

    <!-- 表示やエラー -->

    { "##" } 期待したこと

    <!-- 期待した動作 -->

    { "##" } リビジョンまたはバージョン

    <!-- バージョンやコミット ID -->

    { "##" } ワークスペース

    <!-- 種別だけ -->

    { "##" } ログまたはスクリーンショット

    <!-- 共有できる添付ファイル -->

    { "##" } 診断情報

    - アプリ: { $version }
    - リビジョン: { $revision }
    - OS: { $os }
    - プロトコル: { $protocol }
    - ワークスペース種別: { $kind }

github-auth-title = GitHub 認証
github-auth-connect = GitHub に接続…
github-auth-pending = GitHub での認証を待機中
github-auth-countdown = 待機中 · 残り { $remaining }
github-auth-chat-connected = GitHub 接続済み（{ $login }）
github-auth-copy-url = URL をコピー
github-auth-retry = もう一度試す
github-auth-report = 不具合を報告…
github-auth-switch = アカウントを切り替え…
github-auth-confirm-disconnect = このサーバーの GitHub 接続を解除しますか？
github-auth-confirm-switch = 現在の GitHub 接続を解除して、新しいアカウントで接続し直しますか？
github-auth-cancel = キャンセル
github-auth-title-connected = GitHub と接続中（{ $login }）
github-auth-title-disconnected = GitHub に未接続
