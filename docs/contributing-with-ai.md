# AI と一緒に fuzukue の PR を送る

fuzukue 上の Claude Code や Codex に修正を頼むときは、公開の fuzukue ソースを開いたワークスペースで作業します。再現だけできたときは [Issue フォーム](https://github.com/momota1029/fuzukue/issues/new/choose) の「AI に変更を頼んだ」、大きな機能は「機能の提案」から相談できます。本文には目的、起きたこと、fuzukue の版またはビルド SHA、端末とサーバーの OS を書きます。

## 初回だけ fork する

利用者本人が Mac のターミナルで自分の GitHub アカウントにログインします。既存の fork があれば、そのクローンを再利用します。

```sh
gh auth login --hostname github.com --git-protocol https --web
gh auth setup-git --hostname github.com
gh repo fork momota1029/fuzukue --clone
```

クローンを開き、`origin` が自分の fork、`upstream` が `momota1029/fuzukue` であることを確かめます。既存のクローンで `upstream` がなければ元リポジトリをその名前で登録します。コミット前に、このクローンの Git 作者情報に公開してよい名前と GitHub のプライバシー用 noreply アドレスを設定します。 [GitHub CLI の fork](https://cli.github.com/manual/gh_repo_fork) · [コミット用アドレス](https://docs.github.com/en/account-and-profile/how-tos/email-preferences/setting-your-commit-email-address)

## 変更ごとにブランチを作る

```sh
git fetch upstream main
git switch --no-track -c fix/short-description upstream/main
```

修正は `fix/短い説明`、機能は `feat/短い説明` のように名前を付け、ひとつの目的に絞った小さなコミットを作ります。AI にはファイルの変更、テスト結果、公開用の PR 下書きまで用意してもらいます。

## 公開前に確かめる

- 送るコミットだけを置いたクリーンな作業ツリーで `scripts/pre-pr-check.sh` を実行します。既定の比較先は `upstream/main` です。各コミットの追加行・追加ファイル名・メッセージを検査し、Rust ワークスペースのテストと Cards のチェックを実行します。`TMPDIR` は Git リポジトリの外に置きます。`--privacy-only` で差分検査だけやり直せます。
- デスクトップや Android の変更には該当クライアントの確認を足します。Android の JVM テスト `./gradlew :app:testDebugUnitTest` は SDK と Gradle の準備に時間がかかるため手動で実行します。 [デスクトップの撮影](headless.md) · [Android の手順](../android/README.md#ビルドと確認wsl)
- 差分と全コミットを自分でも見直します。Git の作者情報、画像・添付、PR 本文、ログ、会話の抜粋から、個人のパス・メール、認証情報、非公開ネットワークの名前やアドレス、非公開の資料を除きます。作業中に除いた情報が過去のコミットに残っていないかも確認します。
- [PR テンプレート](../.github/PULL_REQUEST_TEMPLATE.md) に変更と理由、AI を使った範囲、実行したテストと結果、未実施の確認、UI の画像を記入します。対応する Issue やカードがあれば、Issue の URL または `roadmap/cards/R-番号.md` への公開リンクを記載します。マージで解決する Issue は `Fixes #番号` と書けます。

## PR を開く

利用者が確認したブランチと本文だけを公開します。次は `pr_body` に記入済みテンプレートのファイル名を設定した例です。ファイルはクローンの外に置きます。

```sh
branch=$(git branch --show-current)
fork_owner=$(gh api user --jq .login)
git push origin "HEAD:refs/heads/$branch"
gh pr create --repo momota1029/fuzukue --base main \
  --head "$fork_owner:$branch" --title "変更を一行で説明" --body-file "$pr_body"
```

返った PR の URL を利用者に伝えます。メンテナーがリポジトリの **Watch → Custom → Pull requests / Issues** を登録しておけば、GitHub の通知に届きます。メンテナーが CI と Cards を見てレビューし、マージ後は変更を含むベータ版の公開を追えます。Mac のベータ配布は [R-29](../roadmap/cards/R-29.md) で追跡しています。 [GitHub の通知設定](https://docs.github.com/en/subscriptions-and-notifications/get-started/configuring-notifications)
