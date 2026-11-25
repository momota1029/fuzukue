# fuzukue for Android

Android 10 以降のスマホとタブレット向けの指示出しクライアント。Kotlin と Jetpack Compose を使う。[設計カード](../cards/design/android-client.md)

## 接続

1. PC 側の secretaire を Tailscale IP または LAN IP で待ち受けるように設定する。トークンの表示方法と接続元の許可設定は [リモート接続](../docs/remote.md) を参照。
2. アプリの「接続設定」でサーバー URL を入力する。たとえば `ws://your-tailnet-host.example:7878/ws` のホスト名を、実際の IP または名前に置き換える。 `ws` / `wss` / `http` / `https` に対応し、パスを省くと `/ws` を使う。
3. サーバーの許可リストで接続できる端末はトークンを空にできる。それ以外はトークン欄に入力する。トークンは Android Keystore の鍵で暗号化して保存し、WebSocket の Bearer ヘッダーで送る。LAN で `ws://` や `http://` を使う場合は信頼できるネットワークに限る。
4. 「接続」を押す。接続時にサーバーとプロトコルを交渉する。対応範囲は 18〜48 で、更新中の保留表示には 48 が必要。範囲外ならアプリとサーバー応答の番号を表示する。

## 使い方

- ワークスペースを切り替え、ピン留めを先頭にした一覧から既存のスレッドを開くか、新しいスレッドを作る。幅 720 dp 以上では一覧と会話を並べる。
- 入力欄は 5 行まで広がり、それ以降は欄内でスクロールする。応答中は右端が「停止」になり、下書きがあると右端の「送信」とその上の小さな「停止」を使える。「作業中…」「待機中…」の行をタップすると、作業の詳細と作業役ごとの停止操作を開ける。
- 送ったメッセージはサーバーから届くまで「受信確認待ち」と表示する。接続が一時的に切れても下書きと確認待ちのメッセージが残り、再接続で会話を受信すると確認できる。サーバーが更新のために作業を切り上げているときは、受け付けたメッセージを更新後まで保留する。
- 会話には返信のストリーミング、作業カードの担当名・状態・経過時間・進捗要約、方針の次の一手を表示する。承認カードでは PC と同じサーバーの要約を読み、「許可」か「拒否」を選ぶ。
- 会話は簡易 Markdown レンダラーで描画する。見出し、強調、箇条書き、引用、コード、リンク表記に対応する。数式は [RaTeX 0.1.15](https://github.com/erweixin/RaTeX/releases/tag/v0.1.15) でインライン・別行ともに端末内で描画する。描画できない式は等幅の TeX ソースを表示する。
- 「バックグラウンド接続」をオンにすると、前景サービスが選択中のワークスペースの WebSocket を保持する。Android 側で通知を許可すると、バックグラウンドで受信した応答・作業の終了、承認待ち、目標の休止・再開を通知する。Google のサービスは使わない。過去の履歴は接続時に再表示し、通知はライブ受信分だけを対象にする。

### スマホから添付する

入力欄の「＋」から写真・画像、PDF、テキスト、JSON を選べる。端末にカメラアプリがあれば、その場で撮って添付できる。スクリーンショットは Android の共有メニューで fuzukue に送り、端末のキーボードやクリップボードが画像の貼り付けに対応していれば入力欄にも貼り付けられる。スレッドを開いていないときは、共有内容の追加先を一覧から選ぶ。

添付は 1 メッセージに 8 件、1 ファイル 50 MiB まで。画像の形式を確認し、上限を超える画像は元ファイルが 100 MiB 以下なら縮小を試みる。位置情報の EXIF を検出した JPEG は画素を保ったまま位置情報を削除する。縮小時と、それ以外の形式で位置情報を検出したときは、向きを反映して再エンコードする。転送の進捗は入力欄の上に出る。切断などで失敗したら再接続後に再試行できる。転送後に元データの一時キャッシュを削除し、アプリを開いている間は送信済みの画像に小さなプレビューを表示する。

## 配布と更新

初回インストールには [GitHub のベータリリース](https://github.com/momota1029/fuzukue/releases/tag/android-beta) の署名済み APK を使える。自前サーバーから入れる場合は、スマートフォンを許可リストに入っている Tailscale アカウントで tailnet に参加させ、ブラウザーで `https://<サーバーの MagicDNS 名>/android/beta/download.apk` を開く。Tailscale Serve が HTTPS を提供し、secretaire が接続元の Tailscale ログインを許可リストと照合する。URL にトークンは付けない。別のチャネルを配布した場合は `beta` を置き換える。ダウンロードした APK を開き、Android が求めたらブラウザーまたはファイル管理アプリからのインストールを許可する。

tailnet 外の HTTPS プロキシではフィードと APK の両方に `Authorization: Bearer` の端末トークンが必要となる。通常のブラウザーのダウンロードリンクではこのヘッダーを付けられない。tailnet 経由で取得するか、端末トークンを扱える HTTP クライアントで取得した署名済み APK をスマートフォンに移す。

手元でビルドしたデバッグ版がすでに入っている場合は、署名が異なるため、デバッグ版をアンインストールしてからリリース版を入れる。アンインストールすると保存した接続設定が消える。

アプリ ID は `dev.fuzukue.android`。旧 ID `dev.labgui.remote` のインストールは Android では別アプリとして扱われる。新しい APK を手動でインストールし、接続設定を入力し直す。旧アプリはアンインストールできる。

「接続設定」の「更新を確認」は、初期設定では `beta` の配布元を自動で選ぶ。接続先が HTTPS で互換フィードを提供していれば、そのフィードを使う。HTTP の tailnet 接続や、互換フィードが取得できない場合は、[GitHub の `android-beta` フィード](https://github.com/momota1029/fuzukue/releases/download/android-beta/android-latest.json) を確認する。GitHub に端末トークンは送らず、アセットのリダイレクトは HTTPS に限って追う。自前サーバーの認証拒否や TLS エラーは理由を表示する。

`stable` と `beta` は接続先の HTTPS サーバーまたは指定の HTTPS URL も選べる。以前入力した URL は `beta` に引き継ぎ、URL が空の既存の `beta` 設定は自動選択に移行する。接続先サーバーを明示的に選んだ場合は、通信エラー時に、サーバーの対応プロトコルが分かっていれば同じチャネルの設定済み URL を確認する。`dev` は接続先の HTTPS サーバーを使う。自動更新の初期設定はオフ。

自動更新をオンにすると、起動時とアプリを開いている間に新版を確認する。通常は約 1 時間、通信エラー後は約 15 分の間隔を空ける。APK のハッシュ、署名証明書、アプリ ID、増加する `versionCode`、サーバーとの互換性を確かめてから、操作が落ち着いたタイミングで適用する。作業や承認が表示されている間、送信・添付・編集・再接続中は適用を待つ。切断したダウンロードは、強い ETag と取得対象が同じ場合に再開する。

初回の更新では「提供元の設定を開く」から、fuzukue の「この提供元のアプリを許可」をオンにする。戻ると許可を再確認する。Android 12 以降では条件がそろうと自動でインストールする。Android 10 / 11 や、Android が確認を求める場合は、更新欄の「Android の確認画面を開く」から進む。キャンセル後は同じ更新を手動で試せる。

同梱ライブラリのライセンスとソースの案内は「接続設定」→「ライセンス」で確認できる。

### 自前サーバーに配布する

サービスと同じユーザー・状態ディレクトリ設定で実行する。下のプロジェクトパスは secretaire の起動時と同じものを指定する。Android SDK Build Tools、JDK 21 と署名設定はビルドするホストに用意する。

```sh
cargo build --release -p secretaire
(cd android && ./gradlew :app:testDebugUnitTest)
FUZUKUE_SECRETAIRE="$PWD/target/release/secretaire" \
  scripts/release-android.sh --dry-run --server beta /path/to/project
FUZUKUE_SECRETAIRE="$PWD/target/release/secretaire" \
  scripts/release-android.sh --server beta /path/to/project
```

最後にリリースノートのファイルを渡すと、その先頭行がアプリに表示される。コマンドは署名済み APK をビルドし、`apksigner`、`aapt`、SHA-256 で検証する。同じアプリ ID と署名証明書を使い、版を更新するときは全チャネルで別の APK に同じ `versionCode` を割り当てない。検証した APK を版とハッシュで固定した URL に登録してから、チャネルの JSON を原子的に切り替える。直前の URL も引き続き取得できる。稼働中のサーバーは、互換な版を配信する。

APK の登録はこのコマンドで明示的に行う。secretaire の自己更新は既存のフィードを引き継ぎ、稼働版との互換性を再評価する。現行 APK が互換でなく、直前の版が互換なら直前の版を提示する。自己更新に Android SDK や署名鍵は必要ない。端末に入れた APK を修正版に入れ替えるときは、問題版より大きい `versionCode` と同じ署名鍵で復旧版を配布する。

初回インストール用の HTTPS は、`server.toml` の既存の `[remote]` に次を追加し、更新した secretaire を再起動してから Tailscale Serve で設定する。

```toml
[remote]
tailscale_serve_listen = "127.0.0.1:7880"
allowed_logins = ["user@example.com"]
```

```sh
tailscale serve --bg http://127.0.0.1:7880
tailscale serve status
```

`7880` はこのマシンの Tailscale Serve 専用にし、汎用プロキシの転送先には別のポートを使う。Serve の HTTPS ホスト名と、スマートフォンを所有する実際の Tailscale ログインを使う。tailnet 外へ公開する Funnel ではなく、tailnet 内の Serve を使う。[Tailscale Serve](https://tailscale.com/docs/features/tailscale-serve) · [端末トークンと HTTPS プロキシ](../docs/remote.md#tailscale-なしで公開する)

### 署名キーの作り方

初回だけ、リポジトリの外にリリース用の署名キーを作る。`keytool` が尋ねるパスワードを設定し、キーストアとパスワードは安全な場所に保管する。以後の APK は同じキーで署名する。

```sh
install -d -m 700 "$HOME/.android/fuzukue-signing"
keytool -genkeypair -storetype PKCS12 \
  -keystore "$HOME/.android/fuzukue-signing/release.p12" \
  -alias fuzukue -keyalg RSA -keysize 3072 -validity 10000
```

`android/keystore.properties`（Git の除外対象）に次の 4 項目を設定する。`storeFile` は絶対パスか `android/` からの相対パスを使う。PKCS12 では `keyPassword` に `storePassword` と同じ値を設定する。

```properties
storeFile=/absolute/path/to/release.p12
storePassword=<keystore password>
keyAlias=fuzukue
keyPassword=<key password>
```

環境変数を使う場合は、上の順に `FUZUKUE_ANDROID_KEYSTORE`、`FUZUKUE_ANDROID_STORE_PASSWORD`、`FUZUKUE_ANDROID_KEY_ALIAS`、`FUZUKUE_ANDROID_KEY_PASSWORD` を設定する。値が入っている環境変数を優先する。キーストアとパスワードはコミットしない。

### リリースの作り方

1. `android/version.properties` で `versionName` を決め、`versionCode` を前回の配布版より大きくする。Gradle と配布スクリプトは同じファイルから両方の値を読む。
2. UTF-8 のリリースノートを用意し、先頭行にアプリ内で表示する短い要約を書く。`cd android` で `../scripts/check-android-notices.sh` と `./gradlew :app:testDebugUnitTest` を実行する。署名を設定し、公開前に署名済み APK でライセンス表示を実機確認する。GitHub CLI を公開先への書き込み権限があるアカウントで認証し、配布するコミットの作業ツリーから次を実行する。

   ```sh
   export FUZUKUE_ANDROID_RELEASE_REPO=momota1029/fuzukue
   ../scripts/release-android.sh --dry-run /path/to/release-notes.md
   ../scripts/release-android.sh /path/to/release-notes.md
   ```

3. スクリプトは `:app:assembleRelease` を実行して `apksigner` で署名を検証し、`android/build/releases/<version>/` に APK、`.apk.sha256`、`android-latest.json` を出力する。JSON の `package` にはビルド結果の `applicationId` が入る。
4. 通常の実行で `android-beta` に公開する。初回はプレリリースを下書きで作り、すべてのアセットが揃ってから公開する。更新時は同じタグに新しい APK、SHA-256、`android-latest.json` の順でアップロードする。フィードの差し替えに失敗すると以前のフィードの復元を試みる。以前の APK と SHA-256 は取得できる状態で保持する。GitHub の APK 名は `fuzukue-android-<versionName>-<versionCode>.apk`。同じ `versionName` で更新する場合も、`versionCode` ごとに別の URL で取得できる。

配布スクリプトは `--dry-run` でも `scripts/check-android-protocol.sh` と `scripts/check-android-notices.sh` を実行する。dry-run は GitHub への接続やアセットの公開を行わない。ライセンスの確認には JDK 21 を使う。依存を変更したときは APK に入る推移依存も見直し、`app/src/main/assets/licenses/THIRD_PARTY_LICENSES.txt` のバージョンと表示を更新する。各チェッカーはリポジトリのルートから単独で実行できる。

リリースビルドの R8 とリソース縮小は `optimization { enable = false }` で無効にしている。縮小を有効にする際は署名済み APK を実機に入れて接続・更新と RaTeX の描画を確認する。

## ビルドと確認（WSL）

Android SDK は `~/Android/Sdk`（`platforms;android-37.0`、`build-tools;36.0.0`、`platform-tools`）、JDK 21 は `~/opt/jdk-21.0.13.1+2` を使う。

```sh
cd android
echo "sdk.dir=$HOME/Android/Sdk" > local.properties
export JAVA_HOME=$HOME/opt/jdk-21.0.13.1+2
./gradlew :app:assembleDebug :app:testDebugUnitTest :app:recordRoborazziDebug
```

- APK は `app/build/outputs/apk/debug/app-debug.apk` に出る。
- JVM テストでは fuzukue-proto の JSON を読み、差分から会話と作業カードの状態を更新できることを確認する。
- 再接続テストでは、作業の詳細ダイアログ内で作業名と「閉じる」を検索し、入力欄の送信と停止を個別のテストタグで指定する。
- Roborazzi は設定、一覧、進捗付き会話、承認、タブレットの画面をライト・ダークで `app/build/shots/` に出す。更新のテストではプロトコル不一致からの確認、提供元の設定、適用の保留と Android の確認操作を検証する。画像は `app/build/shots/` の `update-github.png`、`update-available.png`、`update-permission.png` に出る。Robolectric では Android 用ネイティブライブラリをロードできないため、数式は TeX ソースで写る。
- 実機では Tailscale / LAN での接続、日本語 IME、通知許可とバックグラウンドでの通知、RaTeX による数式描画を確認する。
