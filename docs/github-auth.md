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
