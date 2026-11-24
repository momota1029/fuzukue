# Windows で使う

サーバーは WSL2、クライアントは Windows で動かす。通常はクライアントから
`ws://127.0.0.1:7878/ws` に接続する。

## 準備と初回セットアップ

- Windows で `wsl --install -d Ubuntu` を実行し、Ubuntu を一度開いてユーザーを作る。
  `wsl -l -v` の VERSION が 2 になっていることを確認し、`wsl --update` で WSL を更新する。
- Windows に Git、Rust の MSVC ツールチェーン、Visual Studio Build Tools の
  「C++ によるデスクトップ開発」（MSVC と Windows SDK）を入れる。
  [WSL の基本コマンド](https://learn.microsoft.com/windows/wsl/basic-commands)、
  [Rust のインストール](https://rust-lang.org/tools/install/)。
- Windows のローカルドライブに fuzukue を clone し、そのディレクトリから PowerShell で実行する。

```powershell
.\scripts\setup-windows.ps1 -WhatIf -Shortcut
.\scripts\setup-windows.ps1 -Shortcut
```

`-WhatIf` は予定を表示する。実行時にはスクリプトが Ubuntu / Debian 用の apt コマンドを表示する。
必要なパッケージを WSL のターミナルで入れてから続行する。WSL に Cargo がなければ、確認を経て
rustup をインストールする。現在のサーバーは Rustls を使うため、通常は `libssl-dev` は不要。

WSL 側では `~/fuzukue` を clone または fast-forward 更新し、`target/release/secretaire` を作る。
Windows 側では `%LOCALAPPDATA%\fuzukue\target\release\fuzukue.exe` を作り、
`settings.toml` の `[server]` を設定する（設定ファイルの場所は下記）。
`-Shortcut` を指定した場合は、スタートメニューの「fuzukue」から開ける。
再実行でビルドと設定を更新できる。セットアップ中はクライアントを閉じておく。

```powershell
.\scripts\setup-windows.ps1 -Distro Ubuntu -Project '/home/alice/my book' -Shortcut
```

- `-RepoUrl` で clone URL を指定できる。省略時は現在のチェックアウトの `origin` を使う。
- `-Distro` を省略すると WSL の既定ディストリビューションを使う。セットアップ後の設定には実際に使った名前が記録される。
- `-Project` を省略すると初回は `~/fuzukue/examples/lab` を開く。同じディストリビューション・リポジトリで再実行すると設定済みのプロジェクトを引き継ぐ。
- Claude Code、Codex、Lean を使う場合は、WSL 側で準備する。自動起動はログインシェルを通さないため、
  `wsl --exec env` の `PATH` から `claude`、`codex`、`lake` などを見つけられるようにする。

## 起動、設定、終了

接続先にすでにサーバーがいれば、そのサーバーを使う。待ち受けがないときは
「WSL でサーバーを起動中…」と表示し、待ち時間を伸ばしながら約 60 秒間接続を試す。
起動に失敗したら「設定」→「サーバー」でエラーと stderr の末尾を確認できる。

設定ペインで値を変更し、「サーバーを再起動」で保存して接続し直す。手動で設定ファイルを編集する場合の例を示す。

通常の設定ファイルは `%APPDATA%\fuzukue\settings.toml` にある。
ポータブルモードを使うには、`fuzukue.exe` と同じディレクトリに `settings.toml` を置く。
ファイルがあれば、クライアントはそこから設定全体を読み込み、設定ペインやセットアップからの変更も同じファイルに保存する。

```toml
[server]
mode = "wsl"
distro = "Ubuntu"
repo = "/home/alice/fuzukue"
project = "/home/alice/my book"
addr = "127.0.0.1:7878"
```

`distro` を空欄にすると WSL の既定を使う。実行中の停止先も同じ指定に従うため、
既定のディストリビューションを切り替える前にクライアントを閉じる。
`FUZUKUE_URL` を指定する場合、WSL モードでは `addr` と同じポートの
`ws://127.0.0.1:ポート/ws`（または `localhost`）に合わせる。
`FUZUKUE_ADDR` は `wsl.exe --exec /usr/bin/env FUZUKUE_ADDR=…` でサーバーに渡す。
引数を個別に渡すため空白を含むパスも使え、Windows の環境変数や `WSLENV` の設定に依存しない。

再起動・通常終了で停止するのは、そのクライアントが起動したサーバーだけ。
Microsoft の WSL 実装では `wsl.exe` の終了後も Linux プロセスが動き続ける場合がある。
クライアントは relay を終了させた後、同じディストリビューションとユーザーで実行ファイル、
プロジェクト、起動時の識別子を照合し、一致した PID に `pkill` で TERM を送る。
約 1 秒で終了しなければ KILL を送る。クライアントの強制終了や WSL の応答停止時には
この処理を完了できず、サーバーが残る場合がある。
[Microsoft の WSL 起動処理](https://github.com/microsoft/WSL/blob/master/doc/docs/technical-documentation/boot-process.md)。

macOS / Linux の自動起動は `mode = "local"` を使い、`project` と必要に応じて
`binary` に絶対パスを指定する。空の `binary` は PATH 上の `secretaire` を使う。

## 外部サーバーに接続する

「設定」→「サーバー」→「外部」を選び、「保存して再接続」を押す。
設定ファイルでは `[server]` の `mode = "external"` に相当する。
モード未設定のクライアントも外部モードで動く。接続先は起動時の `FUZUKUE_URL` で指定する。

```powershell
$env:FUZUKUE_URL = 'ws://127.0.0.1:17878/ws'
& "$env:LOCALAPPDATA\fuzukue\target\release\fuzukue.exe"
```

例えば、別のマシンでループバックにバインドしたサーバーには SSH のローカルポート転送を使える。

```powershell
ssh -N -L 17878:127.0.0.1:7878 user@server
```

## 他の Windows 端末へ配布する

WSL から `scripts/publish-windows.sh /path/to/project` で Windows 版をサーバーに登録する。
接続済みのクライアントは情報欄の通知から更新できる。
初回はクライアントの「端末に送る」で Tailscale の端末を選ぶ。
受信側は送られた `fuzukue.exe` と `settings.toml` を同じフォルダに置いて起動する。
公開、認証、送信待ちの詳細は [クライアントの配布と更新](update.md) を参照。

## タッチスクリーン用の GPUI フォーク

Windows クライアントでは `gpui-pre-windows` と `gpui-pre` の 0.3.12 配布ソースを
`vendor/` に置き、`crates/fuzukue/Cargo.toml` の `[patch.crates-io]` で適用している。
配布版の Direct Manipulation 経路は精密タッチパッド (`PT_TOUCHPAD`) 向けで、
タッチスクリーン (`PT_TOUCH`) のスワイプを GPUI のスクロール入力に変換する処理がなかった。
上流には修正の PR を提出済みで、取り込みまでこのフォークを使う。

Windows 側の `src/events.rs` と `src/window.rs` は、タッチサンプルの実測座標をウィンドウの論理ピクセルに
変換して GPUI 本体へ渡す。Win32 が一つのメッセージにまとめた移動は古い順に処理し、速度推定には
`POINTER_INFO.PerformanceCount` を優先して使う。高精度時刻を取得できない場合は `dwTime`、
メッセージ時刻、処理時刻の順で代用する。

GPUI 本体の `src/interactive.rs` と `src/window.rs` がサンプル時刻を `src/gestures.rs` に渡す。
パンの終了位置には最後に採用した移動座標を使う。パン中の 4 論理ピクセル以内の逆行は無視し、
逆行がそこから 4 ピクセルを超えると方向を切り替える。速度推定は最後の軸方向の移動を起点に
100 ms さかのぼり、サンプル間隔が 40 ms を超えたところで打ち切る。3 点以上なら二次近似、
2 点なら差分で推定する。指を離した時点で最後の軸方向の移動から 80 ms 以上経っている場合は慣性を開始しない。
この判定には実測時刻を使い、末尾の静止サンプルと 4 ピクセル以内の逆行で起点を更新しない。
40 ms と 80 ms を使い分けるのは、端末によって UP の通知が遅れるため。
[Chromium の VelocityTracker](https://chromium.googlesource.com/chromium/src/+/9d52191cb6730215bebb9e7063f0072ee24e103f/ui/events/velocity_tracker/velocity_tracker.cc)

Windows では、最後のパンと同方向で 50 論理ピクセル/秒以上なら慣性を開始する。
初速を 8000 論理ピクセル/秒以下に制限し、速度がゼロまで直線的に低下する曲線を使う。
継続時間は初速を 2300 論理ピクセル/秒²で割り、1.2〜2 秒に収める。距離は初速と継続時間の積の半分で、
300、500、800 論理ピクセル/秒の場合はそれぞれ約 180、300、480 論理ピクセル進む。
2300 と 2 秒の上限は Chromium の Windows 向け曲線を参考にした。1.2 秒の下限は通常のスワイプでも
慣性が見えるように加えた調整。スクロール範囲の端ではスクロール領域が移動量を制限する。
[Chromium の曲線と定数](https://github.com/chromium/chromium/blob/main/ui/events/gestures/physics_based_fling_curve.cc)、
[Windows の既定設定](https://github.com/chromium/chromium/blob/main/ui/base/ui_base_features.cc)

長押しは GPUI が認識し、その操作に対応するウィジェットに渡す。タッチ専用のテキスト選択操作は今回の対象外。

GPUI を更新するときは、対応する版の両クレートを Cargo レジストリから取り直し、
現在の配布版とフォークとの差分を新しいソースに適用する。修正が上流に入ったクレートは、
フォークと対応する `[patch.crates-io]` の項目を外し、クライアントの `Cargo.lock` を更新する。
本体のフォークを外す際は、Windows 側に残っている本体へのパス依存も見直す。
更新後は `crates/fuzukue` で `cargo build --release` を実行し、実機でスワイプの方向と慣性、
タップが一度だけ反応すること、スクロール終了時にクリックが発生しないこと、マウス・ペン・タッチパッドの操作を確認する。

### 実機のタッチトレース

`crates/fuzukue` から Windows PowerShell で次のように起動する。必要に応じてファイル名を変える。

```powershell
$env:FUZUKUE_TOUCH_LOG = Join-Path $env:TEMP 'labo-touch.log'
.\target\release\fuzukue.exe
```

スクロール範囲の端から離れた位置で、普段の速さと速めの上下スワイプ、軽く指を戻してから離すスワイプを試し、
同じ方向に慣性が続くことを確認する。ドラッグして 120 ms ほど指を止めてから離すと、その場で止まることも確認する。
同じ端末で Chrome と比較し、表示倍率を変更した場合も普段の速さで慣性が始まるか確認する。
挙動を再現したらアプリを終了し、`$env:FUZUKUE_TOUCH_LOG` が指すファイルを共有する。
環境変数は `Remove-Item Env:FUZUKUE_TOUCH_LOG` で解除できる。

ログは指定ファイルに追記する。`sample` 行には接触フェーズ、Windows のポインター ID、
GPUI の `touch_id`（未割当は `-1`）、入力時刻（`qpc`、`qpc_hz`、`dw_time_ms`）、
メッセージと処理の時刻、スクリーン上の物理ピクセル座標、合流した履歴サンプルかどうかを記録する。
`release` 行は同じ `touch_id` に対し、スクロール軸上の推定速度（論理ピクセル/秒）、
最後の軸方向の移動からリリースまでの `gap_ms`、および `fling` と `reason` を記録する。
慣性開始時は `started`、開始しなかった場合は `stopped`（80 ms 以上静止）、`too_slow`（速度不足）、
`reversed`（推定速度が逆向き）、`too_few_samples`（有効な時刻が 2 点未満）、
または `not_panning`（タップなどのパン以外の操作）が入る。

## トラブルシューティング

- **WSL 内では待ち受けているが Windows からつながらない。** WSL 側で `ss -ltn`、
  Windows 側で `Test-NetConnection 127.0.0.1 -Port 7878` を実行する。
  NAT モードでは `%USERPROFILE%\.wslconfig` の `[wsl2]` に `localhostForwarding=true` を設定する（既定値）。
  mirrored モードでは IPv4 の `127.0.0.1` を使う。WSL の設定変更を反映するには、作業中のプロセスを終了してから
  `wsl --shutdown` を実行し、クライアントを開き直す。
  [WSL のネットワーク](https://learn.microsoft.com/windows/wsl/networking)、
  [WSL の設定](https://learn.microsoft.com/windows/wsl/wsl-config)。
- **Firewall の確認画面が出る、または VPN・会社の端末で接続できない。** Windows セキュリティで
  対象アプリと現在のプロファイルのルールを確認する。mirrored モードでは Hyper-V Firewall のポリシーも確認する。
  [WSL の接続トラブルシューティング](https://learn.microsoft.com/windows/wsl/troubleshooting)。
- **Windows のビルドが遅い。** `\\wsl$\…` や `\\wsl.localhost\…` はファイルアクセスが
  OS をまたぐ。Defender のリアルタイム検査も多数のビルドファイルへのアクセスに時間がかかる要因になる。
  Windows のソースと Cargo の出力先を Windows のローカルドライブに置き、WSL のソースと出力先は WSL 内に置く。
  [WSL 開発環境でのファイル配置](https://learn.microsoft.com/windows/wsl/setup/environment)、
  [Defender のパフォーマンスモード](https://learn.microsoft.com/windows/dev-drive/)。
