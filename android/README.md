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

初回は GitHub Releases から `fuzukue-android-<version>.apk` をダウンロードし、Android のインストーラーで開く。手元でビルドしたデバッグ版がすでに入っている場合は、署名が異なるため、デバッグ版をアンインストールしてからリリース版を入れる。アンインストールすると保存した接続設定が消える。

アプリ ID は `dev.fuzukue.android`。旧 ID `dev.labgui.remote` のインストールは Android では別アプリとして扱われる。新しい APK を手動でインストールし、接続設定を入力し直す。旧アプリはアンインストールできる。

更新は「接続設定」→「更新を確認」。更新 JSON の `package` が実行中のアプリ ID と一致し、新版があればバージョンとリリースノートの先頭行を表示する。「APK をダウンロードしてインストール」を押すと、アプリが SHA-256、APK のパッケージ名と実行中のアプリ ID、APK と更新 JSON のバージョンコードを照合し、Android の確認画面を開く。Android の設定が開いた場合は、このアプリからのインストールを許可して戻り、「インストール」を押す。ダウンロード中に画面を離れた場合も、戻って「インストール」を押せる。サーバーがアプリの対応範囲より新しいプロトコルを要求した場合も、接続設定に更新の案内が出る。

更新先は初期状態では空欄。自前で配布するときは、生成した `android-latest.json` と、その `apk` 欄にある名前の APK を同じディレクトリに置く。「更新情報の URL」に JSON の HTTPS URL（例 `https://updates.example.org/android/android-latest.json`）を入力して「更新を確認」を押すと、その URL を保存して使う。配布先が変わった場合も同じ欄で切り替える。JSON と APK の取得先は、リダイレクトを含めて HTTPS が必要。

同梱ライブラリのライセンスとソースの案内は「接続設定」→「ライセンス」で確認できる。

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
2. UTF-8 のリリースノートを用意し、先頭行にアプリ内で表示する短い要約を書く。`cd android` で `../scripts/check-android-notices.sh` と `./gradlew :app:testDebugUnitTest` を実行する。署名を設定し、配布するコミットの作業ツリーから次を実行する。

   ```sh
   export FUZUKUE_ANDROID_RELEASE_REPO=OWNER/REPO
   ../scripts/release-android.sh --dry-run /path/to/release-notes.md
   ../scripts/release-android.sh /path/to/release-notes.md
   ```

3. スクリプトは `:app:assembleRelease` を実行して `apksigner` で署名を検証し、`android/build/releases/<version>/` に APK、`.apk.sha256`、`android-latest.json` を出力する。JSON の `package` にはビルド結果の `applicationId` が入る。署名済み APK の `assets/licenses/` に表示ファイルがあり、実機で開けることを確認する。
4. `FUZUKUE_ANDROID_RELEASE_REPO` に配布先の GitHub リポジトリを指定する。スクリプトはこの宛先で使う公開コマンドを表示する。初回は `gh release create` でプレリリース `android-beta` を作る。次回以降は新しい APK と SHA-256 を先にアップロードし、最後に `android-latest.json` とリリースノートを更新する。利用者に案内した更新 URL と新しい APK の取得を確認してから、旧 APK と SHA-256 を `gh release delete-asset` で個別に削除する。

配布スクリプトは `--dry-run` でも `scripts/check-android-protocol.sh` と `scripts/check-android-notices.sh` を実行する。ライセンスの確認には JDK 21 を使う。依存を変更したときは APK に入る推移依存も見直し、`app/src/main/assets/licenses/THIRD_PARTY_LICENSES.txt` のバージョンと表示を更新する。各チェッカーはリポジトリのルートから単独で実行できる。

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
- Roborazzi は設定、一覧、進捗付き会話、承認、タブレットの画面をライト・ダークで `app/build/shots/` に出す。更新のテストはサーバーとのプロトコル不一致から新版を確認できることを検証し、`app/build/shots/update-available.png` に撮影する。Robolectric では Android 用ネイティブライブラリをロードできないため、数式は TeX ソースで写る。
- 実機では Tailscale / LAN での接続、日本語 IME、通知許可とバックグラウンドでの通知、RaTeX による数式描画を確認する。
