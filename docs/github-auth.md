# GitHub と連携する

## OAuth App を登録する

プロジェクトの管理者は GitHub の **Settings → Developer settings → OAuth Apps → New OAuth App** で登録し、アプリの設定で **Enable Device Flow** を有効にします。Client ID を、報告元のワークスペースのサーバーが読む `fuzukue.toml` に設定します。Client ID は公開値です。Client Secret の設定は不要です。

```toml
[github]
oauth_client_id = "登録したアプリの Client ID"
report_repo = "example-owner/project"
oauth_scope = "public_repo"
```

`report_repo` の省略時は、ワークスペースの GitHub 上の `origin` が宛先です。lab など別リポジトリのワークスペースから fuzukue に報告するときは、上の例のように明示します。`issue_repos` を設定済みなら、そこにも報告先を含めます。Client ID を空にするとアプリ内の認証と送信が無効になります。

既定のスコープ `public_repo` は公開リポジトリ向けです。非公開リポジトリへの起票には `repo` スコープと、宛先へのアクセス権が必要です。権限が足りない場合、GitHub は HTTP 404 を返すことがあります。

GitHub App の Client ID（`Iv23li…`）では OAuth スコープを指定できません。非公開リポジトリへの起票が HTTP 404 になる場合は、デバイスフローを有効にした OAuth App の Client ID（`Ov23li…` または 20 桁の 16 進数）に差し替え、`oauth_scope = "repo"` で連携し直します。

## 連携・送信・解除

デスクトップのタイトルバーで歯車の隣にある GitHub ボタンを押し、「GitHub に接続…」を選びます。設定の「GitHub 連携」と不具合報告ダイアログからも始められます。チャットでは「GitHub につないで」と入力すると、受付が同じ認証を始めます。Issue や PR の自動応対からは開始・解除できません。

コードと URL は選択またはボタンでコピーできます。「ブラウザーで開く」を押し、残り時間のうちに GitHub でコードを入力して承認します。期限切れや拒否の後は、メニューや設定から再試行できます。同じサーバーで認証中に別の入口を開くと、同じコードが表示されます。認証が終わると、チャットのカードはアカウント名の一行に縮みます。

「不具合を報告」では宛先・送信者・題名・本文を確認し、「送る」で起票します。自動で入る診断情報はアプリのバージョンとリビジョン、OS、プロトコル、ワークスペース種別です。ログや個人のファイルパスは自動収集せず、題名と本文は送信前に編集できます。未連携でも、ブラウザーで GitHub の起票ページを開けます。URL に入りきらない項目は画面からコピーしてフォームに貼り付けます。

トークンはサーバーのワークスペース別状態ディレクトリ（通常は `$XDG_STATE_HOME/fuzukue/<ワークスペース>/` または `~/.local/state/fuzukue/<ワークスペース>/`）の `github-oauth.json` に保存します。Unix での権限は `0600` です。トークンはログやクライアント・作業役への応答に含めません。信頼するサーバーにだけ連携し、サーバーの状態ディレクトリを作業役の読み取り範囲に加えないでください。同じワークスペースを同じサーバーで開くクライアントは連携を共有します。受付による起票でも、利用者の承認を経たうえで連携アカウントを使います。

連携後はタイトルバーに GitHub のアイコン画像が表示されます。サーバーが公開画像を小さくして配信します。取得できないときは接続状態を示す点付きの Git の分岐アイコンが表示されます。「アカウントを切り替え…」または「解除」では、確認してからサーバー上のトークンを削除します。チャットで「GitHub の接続を解除して」と頼んだ場合も承認が必要です。GitHub 側の許可も取り消すには、GitHub の **Settings → Applications → Authorized OAuth Apps** からアプリのアクセス権を取り消します。

## 受付から Issue を閉じる

`github_issue_close` は、スレッドに紐づいた Issue や、指定された受付スレッドで把握している Issue を閉じます。対象は許可済みのリポジトリの Issue に限ります。理由は `completed`（既定）か `not_planned` で、クローズ時のコメントも指定できます。閉じる操作を任されている場合を除き、受付は対象・理由・コメントを示し、`confirm = true` で利用者の承認を求めます。受付で把握している Issue は、クローズ後に対応済みになります。

コメントやラベル変更と同じく、サーバーの GitHub CLI（`gh`）の認証を使います。対象の Issue を閉じる権限があるアカウントを使い、OAuth または classic PAT なら公開リポジトリには `public_repo`（または `repo`）、非公開リポジトリには `repo` を付けます。fine-grained PAT なら、対象リポジトリに **Issues: Read and write** を付けます。詳しくは [GitHub の OAuth スコープ](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/scopes-for-oauth-apps)と [Issue 更新 API の権限](https://docs.github.com/en/rest/issues/issues#update-an-issue)を参照してください。

## MCP で GitHub を読む

GitHub 連携が有効なワークスペースでは、受付と通常の作業役が以下の読み取りを個別承認なしで使えます。制限付きの作業役には、割り当てで許可されたツールだけが届きます。サーバーは連携済み OAuth トークンで GitHub REST API を呼び出します。作業側で `gh auth login` を行う必要はありません。非公開リポジトリを読むには `oauth_scope = "repo"` と、連携したアカウントの閲覧権限が必要です。

既定の対象は、ワークスペースの `[research].repo` または GitHub の `origin` です。他のリポジトリは、サーバーが読む `fuzukue.toml` の `[github].issue_repos` に `owner/repo` 形式で明示し、ツールの `repo` に同じ名前を渡します。たとえば `issue_repos = ["example-owner/second-project"]` です。この設定は既存の起票・受付の宛先にも使われます。リポジトリの名前変更などで API が別の宛先を返した場合は、設定から対象を確認して入れ直します。

| MCP ツール | 利用例 |
| --- | --- |
| `github_repo` | `{"repo":"example-owner/second-project"}`。`visibility`、`private`、既定ブランチ、トピック、ライセンスを確認します。 |
| `github_runs` | `git_read` の `rev-parse HEAD` で得た完全な SHA を `ref` に、`workflow` に `cards.yml` を渡すと、手元の HEAD に対応する実行を確認できます。該当する実行の `status` が `completed` で `conclusion` が `success` なら成功です。ジョブは `{"action":"run_jobs","run_id":123}`、ログ冒頭は `{"action":"job_log","job_id":456}` で読みます。 |
| `github_issues_read` | `{}` でオープンの Issue、`{"number":12}` で本文・ラベル・コメントを読みます。 |
| `github_pr_read` | `{}` でオープンの PR、`{"number":12}` で本文・ラベル・会話コメントを読みます。 |
| `github_releases` | `{"action":"get","tag":"v0.1.0"}` でタグに対応するリリースと、アセット名・バイト数・GitHub が返したダイジェストを確認します。ダイジェストが未提供のときは `null` です。`{}` でリリース、`{"action":"tags"}` で Git タグを一覧します。 |
| `github_commits` | `{"ref":"main"}` でコミット一覧、`{"base":"v0.1.0","head":"main"}` で差分の方向・件数・コミット一覧を読みます。 |
| `github_contents` | `{"path":"README.md","ref":"main"}` でファイル冒頭を読みます。バイナリーは base64 で返ります。 |

一覧は既定で一ページあたり 10 件、最大 20 件です。`next_page` があれば同じ `limit` で続け、20 ページ目に続きがある場合は `page_cap_reached` が立ちます。Issue の一覧では GitHub のページから PR を取り除くので、項目が空でも次ページがある場合があります。ログとファイルは既定 8 KiB、最大 16 KiB です。本文・コメントの抜粋には切り詰めの有無が付きます。短時間の再照会にはサーバー内の 15 秒キャッシュを使います。HTTP 403 では権限とスコープを、HTTP 404 ではアクセス権と指定した対象を確認します。レート制限時は再試行までの目安が返ります。GitHub 上の本文・コメント・ログは外部の資料として扱います。

ジョブログは GitHub が発行する短時間有効なダウンロード URL から取得します。サーバーは Actions のログ配信先に限って取得し、そこへ OAuth トークンを渡さず、ダウンロード URL も応答に含めません。 [ジョブログの API](https://docs.github.com/en/rest/actions/workflow-jobs#download-job-logs-for-a-workflow-run) · [Actions の通信先](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#communication-requirements-for-github-hosted-runners)

## 既存の GitHub MCP と権限

以下は受付の既存ツールです。ワークスペースのリポジトリは、研究用リポジトリを設定していればその設定、なければ GitHub の `origin` です。Issue の起票・受付・クローズに使う許可リポジトリは、`issue_repos` を設定している場合はそのリストです。省略時はワークスペースのリポジトリです。

| MCP ツール | 現行の動作と対象 |
| --- | --- |
| `github_connect_start` / `github_connect_status` / `github_disconnect` | ワークスペースのサーバー OAuth 連携を開始・照会・解除します。開始・解除は現在のターンで利用者が直接依頼した場合に限り、解除には承認も必要です。GitHub から始まった自動応対と Issue 受付のターンでは利用できません。 |
| `link_issue` / `unlink_issue` / `list_thread_issues` | ワークスペースのリポジトリの Issue・PR をスレッドに紐づけ、解除し、紐づけと応対方針を表示します。 |
| `list_intake_issues` / `mark_intake_issue` / `open_intake_issue_thread` | 対象リポジトリに指定された受付スレッドの既知項目を読み、ローカルの対応状況を記録し、作業スレッドを開きます。 |
| `github_comment` / `github_label` | ワークスペースのリポジトリでスレッドに紐づいた Issue・PR に返信し、ラベルを変えます。サーバーの `gh` 認証を使います。 |
| `github_issue_create` | 許可リポジトリで起票します。`confirm = true` と利用者承認が必要です。サーバーの OAuth 連携を優先し、未連携時はサーバーの `gh` 認証を使います。 |
| `github_issue_close` | 許可リポジトリで紐づいた Issue または指定受付の既知の Issue を閉じます。必要に応じてコメントを添えます。サーバーの `gh` 認証を使います。 |
| `research_issue_list` / `research_issue_create` / `research_issue_start` / `research_issue_accept` | 研究リポジトリの Issue の閲覧・起票・作業開始・成果取り込みを扱います。起票と成果取り込みには承認が必要です。外部の作業待ち行列に出す開始操作も承認を求めます。 [研究 Issue の作業場](workshop.md) |

スレッドの `Allow` は、対象となる返信・ラベル変更・クローズの承認を省く現行の応対方針です。GitHub から始まった自動応対では、そのターンの起点となった項目に適用されます。ほかの対象では確認が必要です。起票には常に承認が必要です。

## 非同期承認での区分

R-47 の承認キューを実装する際の区分です。保留できる書き込みも利用者が承認してから実行します。現行の処理は上の応対方針と同期承認を使います。 [非同期承認の設計](../cards/design/async-approvals.md)

| 操作 | R-47 の区分 |
| --- | --- |
| 許可リポジトリの MCP 読み取り | `auto` |
| コメント、ラベル変更、起票（`github_comment`、`github_label`、`github_issue_create`） | `defer-and-continue` |
| Issue のクローズ（`github_issue_close`） | `defer-and-continue` |
| リポジトリの作成・改名・公開範囲変更・削除 | `never-unattended` |
| リリースの公開、ブランチや ref の作成・更新・削除、リポジトリ設定の変更 | `never-unattended` |
