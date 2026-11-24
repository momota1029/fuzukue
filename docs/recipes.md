# 作業役のレシピ

レシピは、利用者がサーバー設定で許可したコマンドを、作業役や受付の MCP から実行する仕組みです。通常は `server.toml` を編集します。`FUZUKUE_SERVER_CONFIG` で指定した設定ファイル（たとえば `fuzukue.toml`）も使えます。設定ファイルは作業役が編集できる作業ツリーの外に置き、サーバーの起動時に読み込みます。ワークスペースの設定からレシピが有効になることはありません。

## 設定例

次の例は必要なものだけをサーバー設定に追加します。既定で有効なレシピはありません。

```toml
[recipe_runner]
allow_unisolated = false

[recipes.server-test]
command = ["cargo", "test", "--offline", "-p", "secretaire", "{filter}"]
cwd = "."
timeout_secs = 900
max_memory_mib = 8192
queue = "cargo"
args = { filter = { kind = "test-name" } }

[recipes.client-test]
command = ["cargo", "test", "--offline", "-p", "fuzukue", "{filter}"]
cwd = "."
env = { RUST_FONTCONFIG_DLOPEN = "1" }
timeout_secs = 1200
max_memory_mib = 8192
queue = "cargo"
args = { filter = { kind = "test-name" } }
```

`command` は引数配列、`cwd` は呼び出し元ワークスペースからの相対パスです。隔離内ではワークスペースが `/workspace` に見えます。Cargo の依存物は事前に用意します。

`{filter}` は引数配列の要素全体に書き、検証済みの値を一要素に置き換えます。省略時はその要素を取り除きます。`required = true` を指定すると必須になります。`test-name` は 1〜256 文字で、英数字と `_ : . - /` を許可し、先頭の `-` を拒否します。フィルターを位置引数として受け取るコマンドに使います。

## Android の単体テスト

`android-test` は Linux の隔離環境で `:app:testDebugUnitTest` をオフライン実行します。利用者はコードを確認した作業ツリーで、[Android のビルド手順](../android/README.md#ビルドと確認wsl)にある JDK 21、必要な SDK パッケージと承諾済みライセンスを用意し、レシピを動かすホストの端末で次を実行します。

```sh
scripts/stage-android-recipe.sh --dry-run
scripts/stage-android-recipe.sh
```

ヘルパーはネットワークを使う準備実行で Gradle と Robolectric の依存物を集め、JDK・SDK とともに読み取り専用で配置します。既定の配置先は `/usr/local/share/fuzukue-recipe-android` です。作成に管理者権限が必要な場合は表示されたコマンドを利用者が実行します。`--prefix` を指定すると出力される設定にもそのパスが入ります。レシピで使う配置先は `/usr/local/share` など、隔離内から読み取り専用で見える場所に置きます。

新しい Gradle ホームでホスト上のオフライン検証に成功したら、出力された `[recipes.android-test]` を `FUZUKUE_SERVER_CONFIG` が指すマシン固有の `fuzukue.toml` に追加します。既存の同名ブロックがあれば置き換えます。サービスを起動するユーザーでも隔離プローブが通ることを確認し、[通常の手順で再起動](service.md)してから `run_recipe({"name":"android-test"})` を実行します。Gradle Wrapper、JDK、SDK、依存物を変更したときはキューが空の間に再準備します。

ワークスペースのマウントには `android/keystore.properties`、署名キー、認証情報を置かず、ホストの SDK を指す `android/local.properties` もレシピ用の作業ツリーから外します。実行上限は 20 分、メモリ上限は 8 GiB です。メモリ上限はプロセスごとの仮想アドレス空間に適用されます。ホスト上の検証後に、レシピで隔離と上限を含む実行を確認します。

## MCP

- `list_recipes` は名前、引数の宣言、キューの実行・待機状況、参照可能な最近の依頼を返します。
- `run_recipe({"name":"server-test","args":{"filter":"recipes"}})` は同じキューの受け付け順で実行します。同一の呼び出し元、レシピ、引数に待機中または実行中の依頼があれば、その依頼を返します。
- `run_recipe` が約 15 秒以内に完了しない場合は `recipe_status({"request_id":"…"})` で結果を取得します。`cancel_recipe({"request_id":"…"})` は待機中または実行中の依頼を取り消します。
- 依頼は MCP 接続のワークスペースとスレッドに結び付きます。作業役は自分のジョブの依頼を、受付は同じスレッドの作業役の依頼も参照・取り消しできます。
- 結果には状態、終了コードまたはシグナル、実行時間、スキップ・拒否等の理由、最大 8 KiB のログ末尾が含まれます。全ログは `target/recipe-logs/<request_id>.log` に残ります。対象ワークスペースの Git 除外設定に `/target/` を含めてください。サーバー状態の `recipe-audit.jsonl` に依頼と実行の遷移を記録します。再起動時に未完了の依頼は `interrupted` になり、自動再実行は行いません。

`scripts/shot.sh` が終了コード `77` を返した場合はスキップとして扱います。`list_recipes` に利用者が許可した撮影用レシピがあれば `run_recipe` で依頼し、ログに出た PNG を確認します。撮影用レシピではシナリオとワークスペース内の出力先を設定で固定します。撮影用レシピがなければ既存の撮影 MCP を使えます。[ヘッドレス撮影](headless.md#作業役からの撮影)

## 隔離と上限

Linux では `--bind-fd` に対応した bubblewrap とユーザー名前空間が必要です。新しい HOME と環境を用意し、ホストの環境変数を引き継がず、レシピ設定でも `FUZUKUE_*`、`XDG_*`、HOME、PATH、一時ディレクトリ用変数を予約します。ホストの設定・状態ディレクトリをマウントせず、ネットワーク名前空間のループバックと Unix ソケットを利用できます。ワークスペースは書き込み可能です。標準位置の Cargo・Rustup と Gradle の依存キャッシュは一部を読み取り専用で公開します。ホストの認証設定やキャッシュへの書き込みは引き継ぎません。

bubblewrap が見つからない場合は実行を拒否します。`[recipe_runner].allow_unisolated = true` を指定すると、その場合に限り Linux で隔離なしの実行を許可します。テストやビルドのコードはサーバーと同じ OS ユーザーとしてホストのファイルとネットワークにアクセスでき、サーバーの秘密情報や設定も変更できるため、信用できる環境に限って使います。bubblewrap が起動に失敗した場合は拒否を返します。

時間上限は実行開始から計り、超過時と取り消し時はプロセスグループを停止します。隔離時は PID 名前空間も終了します。メモリ上限は Linux の `RLIMIT_AS` によるプロセスごとの仮想アドレス空間の上限です。プロセスツリー全体の cgroup 制限は今後の課題です。隔離なしではプロセスグループを離れた子孫まで停止できる保証はありません。

第 1 段階の承認は利用者によるサーバー設定の編集です。承認 UI、書き込み可能なキャッシュの共有、Windows・WSL の環境差への対応、委譲用 worktree との結び付けは後続です。
